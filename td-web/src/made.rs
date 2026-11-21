//! Pages made through td-page for td-web's tests, as the render thread
//! makes them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::AtomicBool;

use td_alloc::{Budget, BudgetShared, BudgetString};
use td_page::fonts::Measure;
use td_page::page::{Laid, Want};
use td_ui::prose::Metrics;
use td_ui::raster::{Scale, TextFamily};

/// A 400x300 viewport at scale one, styles on, no pictures.
pub(crate) const WANT: Want = Want {
    width: 400.0,
    height: 300.0,
    scale: 1,
    body: TextFamily::Sans,
    styles: true,
    pictures: 0,
};

/// The cell face's measure for `want`.
pub(crate) fn measure(want: Want) -> Measure {
    Measure::new(
        Metrics::new(None, Scale::new(want.scale).unwrap()),
        want.body,
    )
}

/// `text` as a page's markup, charged to `page`.
pub(crate) fn markup(page: &Budget, text: &str) -> BudgetShared<BudgetString> {
    BudgetShared::new(page, BudgetString::copy(page, text).unwrap()).unwrap()
}

/// `text` made into a page for `WANT` and its layout taken.
pub(crate) fn laid(text: &str) -> BudgetShared<Laid> {
    let page = Budget::new(256 << 20).unwrap();
    let made = td_page::pipeline::markup_page(
        markup(&page, text),
        WANT,
        &mut measure(WANT),
        &AtomicBool::new(false),
        &page,
    )
    .unwrap();
    made.laid.expect("a laid page")
}
