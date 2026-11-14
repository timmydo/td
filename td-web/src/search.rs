//! Bounded, fallible search over a page's displayed characters. Results are
//! staged separately so a refused search never replaces the current marks.
use td_alloc::{Budget, BudgetError, BudgetVec};
use td_ui::prose::{self, Items, Metrics};

use crate::document::{Place, Rendered};
use crate::view::Hit;

/// The find entry's maximum number of Unicode scalar values.
pub(crate) const MAX_QUERY: usize = 256;
pub(crate) const MAX_HITS: usize = 10_000;
/// Scratch, result capacity and growth overlap, including budget metadata.
const SEARCH_BYTES: usize = 2 << 20;

#[derive(Default)]
pub(crate) struct Search(Option<Scratch>);
struct Scratch {
    parent: Budget,
    memory: Budget,
    needle: BudgetVec<char>,
    borders: BudgetVec<usize>,
    places: BudgetVec<Place>,
    hits: Items<Hit>,
    compiled: bool,
}
// Scratch is a cache, not part of a view's logical state.
impl PartialEq for Search {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for Search {}
impl std::fmt::Debug for Search {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Search")
            .field(&self.0.as_ref().map(|s| s.memory.used()))
            .finish()
    }
}
impl Search {
    pub(crate) fn prepare(
        &mut self,
        query: &str,
        rendered: &Rendered,
        metrics: &mut Metrics,
    ) -> Result<Items<Hit>, BudgetError> {
        if query.len() > MAX_QUERY * 4 {
            return Err(BudgetError::Limit);
        }
        let count = query.chars().count();
        if count > MAX_QUERY {
            return Err(BudgetError::Limit);
        }
        if count == 0 || rendered.page.lines.is_empty() {
            self.0 = None;
            return Ok(Items::default());
        }
        let parent = rendered.page.budget().ok_or(BudgetError::Limit)?;
        if !self.0.as_ref().is_some_and(|s| s.parent.ptr_eq(parent)) {
            let memory = parent.child(SEARCH_BYTES)?;
            self.0 = Some(Scratch {
                parent: parent.try_clone()?,
                compiled: false,
                needle: BudgetVec::new(&memory)?,
                borders: BudgetVec::new(&memory)?,
                places: BudgetVec::new(&memory)?,
                hits: Items::new(&memory)?,
                memory,
            });
        }
        let scratch = self.0.as_mut().ok_or(BudgetError::Limit)?;
        let Scratch {
            memory,
            needle,
            borders,
            places,
            hits,
            compiled,
            ..
        } = scratch;
        hits.clear();
        if hits.budget().is_none() {
            *hits = Items::new(memory)?;
        }
        if !*compiled
            || !needle
                .iter()
                .copied()
                .eq(query.chars().map(|c| fold(prose::shown(c))))
        {
            *compiled = false;
            needle.clear();
            borders.clear();
            places.clear();
            td_alloc::BudgetVec::try_reserve(needle, count)?;
            td_alloc::BudgetVec::try_reserve(borders, count)?;
            td_alloc::BudgetVec::try_reserve(places, count)?;
            for c in query.chars() {
                needle.try_push(fold(prose::shown(c)))?;
                borders.try_push(0)?;
                places.try_push(Place::default())?;
            }
            let mut k = 0;
            for i in 1..count {
                while k > 0 && needle.get(i) != needle.get(k) {
                    k = borders.get(k - 1).copied().unwrap_or(0);
                }
                if needle.get(i) == needle.get(k) {
                    k += 1;
                }
                if let Some(slot) = borders.get_mut(i) {
                    *slot = k;
                }
            }
            *compiled = true;
        }
        // Only the last `count` positions can begin a complete match. A ring
        // avoids moving a suffix or allocating while characters are scanned.
        for logical in 0..rendered.page.lines.len() {
            let (mut matched, mut next) = (0, 0);
            let mut failed = None;
            rendered.searched(metrics, logical, |scalar, place| {
                let seen = fold(prose::shown(scalar));
                while matched > 0 && needle.get(matched) != Some(&seen) {
                    matched = borders.get(matched - 1).copied().unwrap_or(0);
                }
                if needle.get(matched) == Some(&seen) {
                    matched += 1;
                }
                if let Some(slot) = places.get_mut(next) {
                    *slot = place;
                }
                next += 1;
                if next == count {
                    next = 0;
                }
                if matched < count {
                    return true;
                }
                let hit = Hit {
                    start: places.get(next).copied().unwrap_or(place),
                    end: Place {
                        at: place.at + scalar.len_utf8(),
                        ..place
                    },
                };
                let stored = (|| {
                    if hits.len() == hits.capacity() {
                        let capacity = hits.capacity().saturating_mul(2).clamp(4, MAX_HITS);
                        Items::try_reserve_exact(hits, capacity - hits.len())?;
                    }
                    hits.try_push(hit)
                })();
                if let Err(error) = stored {
                    failed = Some(error);
                    return false;
                }
                matched = 0; // Matches never overlap.
                hits.len() < MAX_HITS
            });
            if let Some(error) = failed {
                return Err(error);
            }
            if hits.len() == MAX_HITS {
                break;
            }
        }
        Ok(std::mem::take(hits))
    }

    /// Keep one reusable, empty result buffer only when it belongs to this
    /// search budget. A view reused on another page cannot charge that page's
    /// search to the previous page's allowance.
    pub(crate) fn recycle(&mut self, mut hits: Items<Hit>) {
        if let Some(scratch) = &mut self.0 {
            if hits.budget().is_some_and(|b| b.ptr_eq(&scratch.memory)) {
                hits.clear();
                scratch.hits = hits;
            }
        }
    }
}

/// Preserve the existing single-scalar case-folding semantics.
fn fold(c: char) -> char {
    let mut lower = char::to_lowercase(c);
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        _ => c,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::document::{Document, Renderer};
    use crate::view::{Selection, View};
    use std::sync::{atomic::AtomicBool, Arc};
    use td_alloc::{with_reservation_limit, Budget};
    use td_ui::raster::{Scale, TextFamily};
    use td_url::Url;

    fn matches(
        query: &str,
        rendered: &Rendered,
        metrics: &mut Metrics,
        _: usize,
    ) -> Result<Items<Hit>, BudgetError> {
        Search::default().prepare(query, rendered, metrics)
    }
    fn metrics() -> Metrics {
        Metrics::new(None, Scale::new(1).unwrap())
    }
    fn document(text: &str) -> Document {
        Document::from_reply(
            Url::parse("https://search.invalid/").unwrap(),
            200,
            Some("text/html; charset=utf-8"),
            text.as_bytes().to_vec(),
        )
    }
    fn state(view: &View) -> (Place, Option<usize>, usize, Option<Selection>, Vec<Hit>) {
        (
            view.top,
            view.focus,
            view.at,
            view.selection,
            view.hits.to_vec(),
        )
    }

    #[test]
    fn bounded_search_agrees_with_a_naive_nonoverlapping_oracle() {
        let mut seed = 9u64;
        for _ in 0..80 {
            let mut text = String::from("<p>");
            for i in 0..300 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                text.push(
                    ['a', 'b', 'A', 'é', 'É', 'İ', '界', '\u{301}', ' '][(seed >> 32) as usize % 9],
                );
                if i % 7 == 0 {
                    text.push_str("<b>x</b>");
                }
                if i % 61 == 0 {
                    text.push_str("</p><p>");
                }
            }
            text.push_str("</p><pre>a\tb aaab abab needle</pre>");
            let doc = document(&text);
            let rendered = doc.render(100, TextFamily::Sans).unwrap();
            let mut search = Search::default();
            let mut previous = Items::default();
            for query in [
                "a", "aa", "aab", "aba", "abab", "é", "İ", "界", "a b", "x", "needle",
            ] {
                let needle: Vec<_> = query.chars().map(|c| fold(prose::shown(c))).collect();
                let mut expected = Vec::new();
                for logical in 0..rendered.page.lines.len() {
                    let mut chars = Vec::new();
                    rendered.searched(&mut metrics(), logical, |c, p| {
                        chars.push((c, p));
                        true
                    });
                    let mut at = 0;
                    while at + needle.len() <= chars.len() {
                        let part = &chars[at..at + needle.len()];
                        if part
                            .iter()
                            .map(|(c, _)| fold(prose::shown(*c)))
                            .eq(needle.iter().copied())
                        {
                            let (c, end) = part.last().copied().unwrap();
                            expected.push(Hit {
                                start: part[0].1,
                                end: Place {
                                    at: end.at + c.len_utf8(),
                                    ..end
                                },
                            });
                            at += needle.len();
                        } else {
                            at += 1;
                        }
                    }
                }
                let hits = search.prepare(query, &rendered, &mut metrics()).unwrap();
                assert_eq!(&*hits, expected, "{query}");
                search.recycle(previous);
                previous = hits;
            }
        }
    }

    #[test]
    fn every_search_allocation_refuses_cleanly_and_returns_credit() {
        let doc = document(&format!("<p>{}</p>", "a ".repeat(MAX_HITS + 10)));
        let rendered = doc.render(800, TextFamily::Sans).unwrap();
        let memory = rendered.page.budget().unwrap();
        let base = memory.used();
        let mut passed = false;
        for n in 0..64 {
            let result = with_reservation_limit(n, || matches("a", &rendered, &mut metrics(), 0));
            if let Ok(hits) = result {
                assert_eq!(hits.len(), MAX_HITS);
                assert_eq!(hits.capacity(), MAX_HITS);
                assert!(memory.used() > base);
                assert!(hits.budget().unwrap().used() <= SEARCH_BYTES);
                drop(hits);
                passed = true;
            }
            assert_eq!(memory.used(), base, "allowance {n}");
            if passed {
                break;
            }
        }
        assert!(passed);
        let hits = matches("a", &rendered, &mut metrics(), 0).unwrap();
        let held = memory.used();
        let other = matches("a", &rendered, &mut metrics(), hits.len()).unwrap();
        assert!(memory.used() > held, "both generations remain charged");
        drop(other);
        assert_eq!(memory.used(), held);
        drop(hits);
        assert_eq!(memory.used(), base);
        assert!(matches(&"界".repeat(MAX_QUERY), &rendered, &mut metrics(), 0).is_ok());
        assert_eq!(
            matches(&"界".repeat(MAX_QUERY + 1), &rendered, &mut metrics(), 0),
            Err(BudgetError::Limit)
        );
        assert!(
            with_reservation_limit(0, || matches("", &rendered, &mut metrics(), MAX_HITS))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_refused_find_preserves_marks_and_navigation_even_during_reveal() {
        let doc = document(&format!(
            "<p>alpha</p>{}<p>beta beta</p>",
            "<p>spacer spacer</p>".repeat(30)
        ));
        let snapshot = |view: &View| state(view);
        let mut expected = View::default();
        let mut laid = doc.render(120, TextFamily::Sans).unwrap();
        expected.find("alpha", &mut laid, &mut metrics(), 4);
        expected.find("beta", &mut laid, &mut metrics(), 4);
        let wanted = snapshot(&expected);
        let mut passed = false;
        let mut late = false;
        for n in 0..1024 {
            let mut rendered = doc.render(120, TextFamily::Sans).unwrap();
            let mut view = View::default();
            view.find("alpha", &mut rendered, &mut metrics(), 4);
            let before = snapshot(&view);
            with_reservation_limit(n, || view.find("beta", &mut rendered, &mut metrics(), 4));
            if rendered.refused() {
                assert_eq!(snapshot(&view), before, "allowance {n}");
                late |= n > 5;
                view.find("beta", &mut rendered, &mut metrics(), 4);
                assert!(!rendered.refused());
            } else {
                passed = true;
            }
            assert_eq!(snapshot(&view), wanted, "allowance {n}");
            if passed {
                break;
            }
        }
        assert!(passed && late);
    }

    #[test]
    fn warmed_search_reuses_capacity_and_rebinds_on_a_different_page() {
        let first = document("<p>alpha beta beta</p>");
        let mut rendered = first.render(800, TextFamily::Sans).unwrap();
        let mut view = View::default();
        view.find("alpha", &mut rendered, &mut metrics(), 4);
        view.find("beta", &mut rendered, &mut metrics(), 4);
        let used = rendered.page.budget().unwrap().used();
        with_reservation_limit(0, || {
            view.find("alpha", &mut rendered, &mut metrics(), 4);
            assert!(!rendered.refused());
            view.find("beta", &mut rendered, &mut metrics(), 4);
            assert!(!rendered.refused());
        });
        assert_eq!(rendered.page.budget().unwrap().used(), used);
        let parent = rendered.page.budget().unwrap().try_clone().unwrap();
        let second = document("<p>beta</p>");
        let mut other = second.render(800, TextFamily::Sans).unwrap();
        let before = state(&view);
        with_reservation_limit(0, || view.find("beta", &mut other, &mut metrics(), 4));
        assert!(other.refused());
        assert_eq!(state(&view), before);
        view.find("beta", &mut other, &mut metrics(), 4);
        assert!(!other.refused());
        assert_eq!(view.hits.len(), 1);
        drop(first);
        drop(rendered);
        assert_eq!(
            parent.used(),
            Budget::metadata_bytes(),
            "no buffer charged to the old page survives"
        );
        view.clear_marks();
        assert!(view.hits.budget().is_none());
    }

    #[test]
    fn root_pressure_and_surviving_result_owners_keep_their_charges() {
        let root = crate::memory::root().unwrap();
        let renderer = Renderer::with_budget(&root).unwrap();
        let baseline = root.used();
        let doc = renderer
            .document(
                Url::parse("https://search.invalid/").unwrap(),
                200,
                Some("text/plain"),
                b"alpha beta".to_vec(),
                &Arc::new(AtomicBool::new(false)),
            )
            .unwrap();
        let rendered = doc.render(800, TextFamily::Sans).unwrap();
        let blocker = root.charge(root.limit() - root.used()).unwrap();
        assert_eq!(
            matches("alpha", &rendered, &mut metrics(), 0),
            Err(BudgetError::Limit)
        );
        drop(blocker);
        let hits = matches("alpha", &rendered, &mut metrics(), 0).unwrap();
        drop(rendered);
        drop(doc);
        assert_eq!(hits.len(), 1);
        assert!(root.used() > baseline);
        drop(hits);
        assert_eq!(root.used(), baseline);
        drop(renderer);
        assert_eq!(root.used(), Budget::metadata_bytes());
    }
}
