//! A page's sheets (td-web/DESIGN.md §10, "Sources"): td-css's user-agent sheet,
//! td-web's own, then the page's own in cascade order with their media,
//! compiled into one set for an environment.

use crate::gather::{Given, Sheets};
use crate::tree::{Sheet::Inline, Tree};
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

/// The page's own sheets in cascade order, cut at the set's count and
/// bytes: each one past them is left out, counted, in that order.
struct Cut<'a> {
    kept: td_alloc::BudgetVec<Given<'a>>,
    count: usize,
    total: usize,
    left_out: usize,
}

impl<'a> Cut<'a> {
    fn new(memory: &Budget) -> Result<Self, td_alloc::BudgetError> {
        Ok(Self {
            kept: td_alloc::BudgetVec::with_capacity(memory, MAX_SHEETS - USER_AGENT.len())?,
            count: USER_AGENT.len(),
            total: USER_AGENT
                .iter()
                .fold(0usize, |sum, sheet| sum.saturating_add(sheet.len())),
            left_out: 0,
        })
    }

    fn take(&mut self, sheet: Given<'a>) -> Result<(), td_alloc::BudgetError> {
        let own = sheet.bytes();
        // A sheet past td-css's own bound is refused alone, not counted.
        let counted = if own > td_css::MAX_INPUT_BYTES {
            0
        } else {
            own
        };
        if self.count >= MAX_SHEETS || self.total.saturating_add(counted) > MAX_STYLE_BYTES {
            self.left_out += 1;
            return Ok(());
        }
        self.count += 1;
        self.total = self.total.saturating_add(counted);
        self.kept.try_push(sheet)
    }
}

/// The user-agent sheets alone, or with the page's own when `own` is set,
/// compiled for `environment` under `memory`: `sheets` in cascade order,
/// else the tree's `style` elements. The page's sheets past the set's
/// count or bytes are left out first, in cascade order.
pub fn compile(
    tree: &Tree,
    sheets: Option<&Sheets>,
    own: bool,
    environment: &Environment,
    memory: &Budget,
) -> Result<(StyleSet, Omitted), td_css::Error> {
    let mut cut = Cut::new(memory).map_err(td_css::Error::Memory)?;
    match (own, sheets) {
        (false, _) => {}
        (true, Some(sheets)) => sheets
            .each(tree, |sheet| cut.take(sheet))
            .map_err(td_css::Error::Memory)?,
        (true, None) => {
            for &id in tree.sheets() {
                if let Some(Inline { text, media }) = tree.sheet(id) {
                    cut.take(Given::inline(text, media))
                        .map_err(td_css::Error::Memory)?;
                }
            }
        }
    }
    let mut sheets = td_alloc::BudgetVec::with_capacity(memory, USER_AGENT.len() + cut.kept.len())
        .map_err(td_css::Error::Memory)?;
    for source in USER_AGENT {
        sheets
            .try_push(Sheet::new(Origin::UserAgent, source))
            .map_err(td_css::Error::Memory)?;
    }
    for given in cut.kept.iter() {
        let sheet = Sheet::new(Origin::Author, given.text);
        let sheet = match given.lists() {
            [] => sheet,
            lists => sheet.with_media(lists),
        };
        sheets.try_push(sheet).map_err(td_css::Error::Memory)?;
    }
    let (set, refused) = StyleSet::from_sheets(&sheets, environment, memory)?;
    let omitted = Omitted {
        left_out: cut.left_out,
        refused: refused.len(),
    };
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

/// Whether the page's own sheets, its `style` elements and those
/// `sheets` gathered, or its `style` attributes set any colour, by a scan
/// for the properties that can (td-web/DESIGN.md §10, "Colours").
pub fn coloured(tree: &Tree, sheets: Option<&Sheets>) -> bool {
    let inline = tree
        .sheets()
        .iter()
        .filter_map(|&id| match tree.sheet(id) {
            Some(Inline { text, .. }) => Some(text),
            _ => None,
        })
        .any(names_colour);
    inline
        || sheets.is_some_and(|sheets| sheets.texts().any(names_colour))
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
        let (_, omitted) = compile(&tree, None, true, &environment, &page).unwrap();
        assert_eq!(omitted.left_out, 70 - (MAX_SHEETS - 2));
        // One sheet past td-css's own bound is refused alone; the rest of
        // the bytes still count against the set.
        let big = "a{}".repeat(td_css::MAX_INPUT_BYTES / 3 + 1);
        let markup = format!("<style>{big}</style><style>b{{color:red}}</style>");
        let (tree, page) = parsed(&markup);
        let (_, omitted) = compile(&tree, None, true, &environment, &page).unwrap();
        assert_eq!(
            omitted,
            Omitted {
                left_out: 0,
                refused: 1
            }
        );
        let (_, omitted) = compile(&tree, None, false, &environment, &page).unwrap();
        assert_eq!(omitted, Omitted::default(), "the user-agent sheets alone");
    }

    #[test]
    fn colour_is_found_in_sheets_and_style_attributes() {
        let coloured = |markup: &str| coloured(&parsed(markup).0, None);
        assert!(!coloured("<style>p{margin:0}</style><p style=width:1px>"));
        assert!(coloured("<style>p{COLOR:red}</style>"));
        assert!(coloured("<p style='background:#fff'>"));
        assert!(coloured("<p style='border:1px solid'>"));
        // A linked sheet's text, once gathered.
        let (tree, page) = parsed("<link rel=stylesheet href='data:text/css,p{background:red}'>");
        let url = td_url::Url::parse("https://example.org/").unwrap();
        let sheets = Sheets::new(&tree, &url, td_charset::Encoding::Utf8, &page).unwrap();
        assert!(!super::coloured(&tree, None));
        assert!(super::coloured(&tree, Some(&sheets)));
    }

    #[test]
    fn a_sheets_media_chain_counts_as_td_css_counts_it() {
        let given = Given::inline("ab", Some(""));
        assert_eq!(given.lists(), [""]);
        assert_eq!(given.bytes(), 2 + 1);
        assert_eq!(Given::inline("ab", None).bytes(), 2);
        // Filled to a few bytes short of the set's, a sheet whose list
        // passes them is left out where its text alone would fit.
        let memory = Budget::new(1 << 20).unwrap();
        let mut cut = Cut::new(&memory).unwrap();
        let room = MAX_STYLE_BYTES - cut.total;
        let quarter = "a".repeat((room - 8) / 4);
        for _ in 0..4 {
            cut.take(Given::inline(&quarter, None)).unwrap();
        }
        let left = MAX_STYLE_BYTES - cut.total;
        let list = "x".repeat(left);
        cut.take(Given::inline("b", Some(&list))).unwrap();
        assert_eq!(cut.left_out, 1);
        cut.take(Given::inline("b", None)).unwrap();
        assert_eq!((cut.left_out, cut.kept.len()), (1, 5));
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
