//! Find (td-web/DESIGN.md §10, "Places, find, selection and links"): each
//! paragraph's text as td-layout displays it, spaces collapsed, a tab
//! taken as a space, a replaced scalar as U+FFFD, an image's `alt` text
//! in its place and a wrap's white space put back as a copy puts it,
//! hidden text left out, searched ignoring case with each character
//! looked at once (Knuth, Morris and Pratt), never past a paragraph nor
//! across a nested box (`text::Event::Break`), at most `MAX_HITS`
//! matches, all in a scratch budget of the page's.

use std::ops::ControlFlow;

use crate::page::Laid;
use crate::text::{self, Event, Place};
use td_alloc::{Budget, BudgetError, BudgetVec};
use td_ui::prose;

pub const MAX_QUERY: usize = 256;
pub const MAX_HITS: usize = 10_000;
/// Find's scratch and matches, a child of the page's budget.
pub const SEARCH_BYTES: usize = 2 << 20;

/// A match, from the place its first scalar starts to where its last
/// ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hit {
    pub start: Place,
    pub end: Place,
}

/// One scalar folded for matching: a tab as a space, another control as
/// U+FFFD, then its lowercase when that is one scalar.
fn fold(c: char) -> char {
    let c = prose::shown(c);
    let mut lower = char::to_lowercase(c);
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

/// The matches of `query` in `laid`, in tree order (a float's lines,
/// searched where they lie, sorted into it); none for an empty query. A
/// refusal of the scratch budget leaves nothing behind.
pub fn find(laid: &Laid, query: &str, page: &Budget) -> Result<BudgetVec<Hit>, BudgetError> {
    let memory = page.child(SEARCH_BYTES.saturating_add(Budget::metadata_bytes()))?;
    let mut hits = BudgetVec::new(&memory)?;
    let count = query.chars().take(MAX_QUERY).count();
    if count == 0 {
        return Ok(hits);
    }
    let mut needle = td_alloc::BudgetVec::with_capacity(&memory, count)?;
    for c in query.chars().take(MAX_QUERY) {
        needle.try_push(fold(c))?;
    }
    // Knuth, Morris and Pratt's failure function.
    let mut borders = BudgetVec::filled(&memory, count, 0usize)?;
    let mut k = 0usize;
    for i in 1..count {
        let c = needle.get(i).copied();
        while k > 0 && needle.get(k).copied() != c {
            k = borders.get(k - 1).copied().unwrap_or(0);
        }
        if needle.get(k).copied() == c {
            k += 1;
        }
        if let Some(slot) = borders.get_mut(i) {
            *slot = k;
        }
    }
    // Where each of the last `count` scalars started, by position mod count.
    let mut starts = BudgetVec::filled(&memory, count, Place::default())?;
    let mut matched = 0usize;
    let mut seen = 0usize;
    let mut failed = None;
    // A scalar, or none where a paragraph ends, which no match crosses.
    let mut step = |c: Option<char>, start: Place, end: Place| -> ControlFlow<()> {
        let Some(c) = c.map(fold) else {
            matched = 0;
            return ControlFlow::Continue(());
        };
        if let Some(slot) = starts.get_mut(seen % count) {
            *slot = start;
        }
        seen += 1;
        while matched > 0 && needle.get(matched).copied() != Some(c) {
            matched = borders.get(matched - 1).copied().unwrap_or(0);
        }
        if needle.get(matched).copied() == Some(c) {
            matched += 1;
        }
        if matched == count {
            let first = starts.get((seen - count) % count).copied().unwrap_or(start);
            if hits.len() >= MAX_HITS {
                return ControlFlow::Break(());
            }
            if let Err(error) = hits.try_push(Hit { start: first, end }) {
                failed = Some(error);
                return ControlFlow::Break(());
            }
            // Matches do not overlap.
            matched = 0;
        }
        ControlFlow::Continue(())
    };
    let _ = text::walk(laid, None, 0, |event| match event {
        Event::Scalar(c, s, e, _) => step(Some(c), s, e),
        Event::Alt(alt, s, e) => {
            for c in alt.chars() {
                step(Some(c), s, e)?;
            }
            ControlFlow::Continue(())
        }
        Event::Gap(gap, s, e) => {
            for c in gap.chars() {
                step(Some(c), s, e)?;
            }
            ControlFlow::Continue(())
        }
        Event::Begin(_) => ControlFlow::Continue(()),
        // No match spans a nested box.
        Event::Break => step(None, Place::END, Place::END),
        Event::End => step(None, Place::END, Place::END),
    });
    match failed {
        Some(error) => Err(error),
        None => {
            hits.sort_unstable_by_key(|hit| hit.start);
            Ok(hits)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::page::tests::laid;

    fn found(markup: &str, query: &str) -> Vec<String> {
        let laid = laid(markup);
        let page = Budget::new(64 << 20).unwrap();
        let hits = find(&laid, query, &page).unwrap();
        hits.iter()
            .map(|hit| {
                let mut out = String::new();
                let _ = text::copy(&laid, hit.start, hit.end, &mut |t| {
                    out.push_str(t);
                    ControlFlow::Continue(())
                });
                out
            })
            .collect()
    }

    /// Matches in a float met within a line come between those before
    /// and after it on that line.
    #[test]
    fn matches_around_a_float_are_in_tree_order() {
        let markup = "<p>ab cd <span style=float:right;width:60px>ab ef</span> ab gh";
        assert_eq!(found(markup, "ab"), ["ab", "ab", "ab"]);
        let laid = laid(markup);
        let page = Budget::new(64 << 20).unwrap();
        let hits = find(&laid, "ab", &page).unwrap();
        assert!(hits.windows(2).all(|w| w[0].end <= w[1].start), "{hits:?}");
    }

    /// Matches in an inline-block are found after those of the line
    /// holding it, and sorted into tree order; its text is a paragraph of
    /// its own, so no match spans its edge.
    #[test]
    fn matches_in_an_inline_block_are_in_tree_order() {
        let markup = "<p>ab cd <span style=display:inline-block>ab ef</span> ab gh";
        assert_eq!(found(markup, "ab"), ["ab", "ab", "ab"]);
        let laid = laid(markup);
        let page = Budget::new(64 << 20).unwrap();
        let hits = find(&laid, "ab", &page).unwrap();
        assert!(hits.windows(2).all(|w| w[0].end <= w[1].start), "{hits:?}");
        assert!(found(markup, "cd ab").is_empty());
        assert_eq!(found(markup, "ab ef"), ["ab ef"]);
    }

    /// No match spans an inline-block, its placeholder, or the place a
    /// float's lines were met: the text either side is apart.
    #[test]
    fn no_match_spans_a_nested_box() {
        // A float or inline-block whose lines hold one with no item.
        for (markup, needle) in [
            ("<p>foo<span style=float:left><br>QQ</span>bar", "oob"),
            (
                "<p>foo<span style=float:left>a<br><br>b</span>mid\
                 <span style=float:left>QQ</span>bar",
                "midbar",
            ),
            (
                "<p>foo<span style=\"display:inline-block;visibility:hidden\"><br>\
                 <span style=visibility:visible>QQ</span></span>bar",
                "oob",
            ),
        ] {
            assert!(found(markup, needle).is_empty(), "{markup}");
        }
        assert_eq!(
            found("<p>xa<span style=float:left><br>aa</span>ax", "aa"),
            ["aa"]
        );
        for markup in [
            "<p>foo<span style=display:inline-block>QQ</span>bar",
            "<p>foo<span style=float:left>QQ</span>bar",
        ] {
            assert!(found(markup, "oob").is_empty(), "{markup}");
            assert!(found(markup, "ooqqb").is_empty(), "{markup}");
            assert_eq!(found(markup, "qq"), ["QQ"], "{markup}");
            assert_eq!(found(markup, "foo"), ["foo"], "{markup}");
        }
        for markup in [
            "<p>foo <span style=display:inline-block>QQ</span> bar",
            "<p>foo <span style=float:left>QQ</span> bar",
        ] {
            assert!(found(markup, "foo bar").is_empty(), "{markup}");
            assert!(found(markup, "o  b").is_empty(), "{markup}");
            assert_eq!(found(markup, "bar"), ["bar"], "{markup}");
            let laid = laid(markup);
            let page = Budget::new(64 << 20).unwrap();
            let hits = find(&laid, "o", &page).unwrap();
            assert!(hits.windows(2).all(|w| w[0].end <= w[1].start), "{hits:?}");
        }
        // An empty inline-block, with no lines, is its placeholder.
        let markup = "<p>foo<span style=display:inline-block;width:9px></span>bar";
        assert!(found(markup, "oob").is_empty());
        assert_eq!(found(markup, "bar"), ["bar"]);
        // A float met mid-word on a wrapped line still breaks it there.
        let markup = "<p style=width:60px>aaaa bbbb cc<span style=float:right>Z</span>dd";
        assert!(found(markup, "ccdd").is_empty());
        assert_eq!(found(markup, "dd"), ["dd"]);
    }

    #[test]
    fn matches_ignore_case_and_stay_within_a_paragraph() {
        assert_eq!(found("<p>Aba ABA aba", "aba"), ["Aba", "ABA", "aba"]);
        assert_eq!(found("<p>aaa", "aa"), ["aa"]);
        assert!(found("<p>ab<p>cd", "bc").is_empty());
        assert_eq!(found("<p>a <b>b</b>  c", "a b c"), ["a b c"]);
        assert_eq!(
            found("<p>see <img alt=Picture> here", "picture here"),
            ["Picture here"]
        );
        assert!(found("<p>hidden <span style=display:none>gone</span>", "gone").is_empty());
        assert!(found("<p>x", "").is_empty());
    }

    /// Text laid but not painted is not found, as text not laid is not.
    #[test]
    fn hidden_text_is_not_found() {
        let markup = "<p>seen needle<p style=visibility:hidden>hidden needle\
                      <p style=display:none>gone needle";
        assert_eq!(found(markup, "needle"), ["needle"]);
        assert!(found("<p>a <span style=visibility:hidden>b</span> c", "b").is_empty());
        assert!(found("<p><img alt=picture style=visibility:hidden>", "picture").is_empty());
    }

    /// A tab is found as a space, as td-ui shows one, and a query's tab
    /// as one too.
    #[test]
    fn a_tab_is_found_as_a_space() {
        assert_eq!(found("<pre>a\tb c</pre>", "a b"), ["a\tb"]);
        assert_eq!(found("<pre>a b\tc</pre>", "b\tc"), ["b\tc"]);
        // A paragraph's end still stops a match.
        assert!(found("<pre>a\nb</pre>", "a b").is_empty());
    }

    #[test]
    fn a_refusal_leaves_nothing_charged() {
        let laid = laid("<p>abc abc");
        let page = Budget::new(64 << 20).unwrap();
        let before = page.used();
        assert!(td_alloc::with_reservation_limit(0, || find(&laid, "abc", &page)).is_err());
        assert_eq!(page.used(), before);
        let hits = find(&laid, "abc", &page).unwrap();
        assert_eq!(hits.len(), 2);
        drop(hits);
        assert_eq!(page.used(), before);
    }

    #[test]
    fn at_most_max_hits() {
        let markup = format!("<p>{}", "a ".repeat(MAX_HITS + 10));
        let laid = laid(&markup);
        let page = Budget::new(64 << 20).unwrap();
        assert_eq!(find(&laid, "a", &page).unwrap().len(), MAX_HITS);
    }
}
