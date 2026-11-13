#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use td_css::cascade::{Origin, StyleSet, HTML_UA_CSS};
use td_css::values::{Color, ComputedStyle, ComputedValue as V, Length, Property};
fn subject<'t, 'd>(tree: &'t HtmlTree<'d>, id: &str) -> ElementRef<'t, 'd> {
    tree.elements()
        .find(|e| e.attribute("id") == Some(id))
        .unwrap()
}
#[test]
fn document_navigation_filters_non_elements_and_detached_foreign_content() {
    let doc=td_html::document::parse("<div id=p> <!--x--> <span id=a></span>text<!--z--><i id=b></i><svg><foreignObject><p id=foreign></p></foreignObject></svg><template><p id=hidden></template><b id=c></b></div>").unwrap();
    let tree = HtmlTree::new(&doc).unwrap();
    let p = subject(&tree, "p");
    let a = subject(&tree, "a");
    let b = subject(&tree, "b");
    let c = subject(&tree, "c");
    assert_eq!(p.first_child().unwrap().node_id(), a.node_id());
    assert_eq!(a.next_sibling().unwrap().node_id(), b.node_id());
    assert_eq!(b.previous_sibling().unwrap().node_id(), a.node_id());
    assert_eq!(b.parent().unwrap().node_id(), p.node_id());
    assert_eq!(c.previous_sibling().unwrap().tag_name(), "template");
    assert!(p.has_text());
    assert!(!a.has_text());
    assert!(!tree
        .elements()
        .any(|e| matches!(e.attribute("id"), Some("foreign" | "hidden"))));
    let order = tree.elements().map(|e| e.node_id()).collect::<Vec<_>>();
    for e in tree.elements() {
        if let Some(parent) = e.parent() {
            assert!(
                order.iter().position(|id| *id == parent.node_id()).unwrap()
                    < order.iter().position(|id| *id == e.node_id()).unwrap()
            );
        }
    }
}
#[test]
fn repaired_parent_order_is_not_arena_order() {
    let doc = td_html::document::parse("<b id=b><div id=d>x</b>y").unwrap();
    let tree = HtmlTree::new(&doc).unwrap();
    let order = tree.elements().map(|e| e.node_id()).collect::<Vec<_>>();
    assert!(
        order
            .windows(2)
            .any(|pair| pair[0].index() > pair[1].index())
            || doc
                .nodes()
                .any(|(id, n)| n.parent().is_some_and(|p| p.index() > id.index()))
    );
    for e in tree.elements() {
        if let Some(parent) = e.parent() {
            assert!(
                order.iter().position(|id| *id == parent.node_id()).unwrap()
                    < order.iter().position(|id| *id == e.node_id()).unwrap()
            );
        }
    }
}
#[test]
fn parse_and_cascade_ua_author_inline_and_inheritance() {
    let document=td_html::document::parse("<!doctype html><html><body><main id=main><p class=lead id=first style='margin-left:2em'>a <a href=/ id=link>link</a></p><!--gap--><p id=second>b</p><ul><li id=item>c</li></ul></main></body></html>").unwrap();
    let tree = HtmlTree::new(&document).unwrap();
    let styles=StyleSet::new(&[(Origin::UserAgent,HTML_UA_CSS),(Origin::Author,"html{font-size:20px} main {color: red;--gap:3px} p.lead {font-size:150%;padding-left:calc(1em + var(--gap))} p + p {color:blue} a:link{color:inherit} a:visited {color:green}")]).unwrap();
    let mut computed = vec![None::<ComputedStyle>; document.len()];
    let mut root_size = 16.;
    for e in tree.elements() {
        let parent = e
            .parent()
            .and_then(|p| computed.get(p.node_id().index()))
            .and_then(Option::as_ref);
        let value = styles
            .compute(&e, parent, root_size, e.attribute("style").unwrap_or(""))
            .unwrap();
        if e.parent().is_none() {
            root_size = value.font_size();
        }
        computed[e.node_id().index()] = Some(value);
    }
    let value = |id: &str, p: Property| {
        computed[subject(&tree, id).node_id().index()]
            .as_ref()
            .unwrap()
            .get(p)
            .cloned()
    };
    assert_eq!(value("first", Property::Display), Some(V::Keyword("block")));
    assert_eq!(
        value("first", Property::FontSize),
        Some(V::Length(Length::Px(30.)))
    );
    assert_eq!(
        value("first", Property::MarginLeft),
        Some(V::Length(Length::Px(60.)))
    );
    assert_eq!(
        value("first", Property::PaddingLeft),
        Some(V::Length(Length::Px(33.)))
    );
    assert_eq!(
        value("link", Property::Color),
        Some(V::Color(Color {
            red: 255,
            green: 0,
            blue: 0,
            alpha: 255
        }))
    );
    assert_eq!(
        value("second", Property::Color),
        Some(V::Color(Color {
            red: 0,
            green: 0,
            blue: 255,
            alpha: 255
        }))
    );
    assert_eq!(
        value("item", Property::Display),
        Some(V::Keyword("list-item"))
    );
}
#[test]
fn dynamic_states_are_caller_owned_and_visited_never_matches() {
    struct States {
        focus: NodeId,
        checked: NodeId,
    }
    impl StateProvider for States {
        fn state(&self, id: NodeId, state: State) -> bool {
            match state {
                State::Hover => true,
                State::Focus => id == self.focus,
                State::Checked => id == self.checked,
            }
        }
    }
    let doc =
        td_html::document::parse("<a id=a href=''>x</a><input id=i checked><div id=d href='/'>")
            .unwrap();
    let plain = HtmlTree::new(&doc).unwrap();
    let a = subject(&plain, "a").node_id();
    let i = subject(&plain, "i").node_id();
    assert!(!subject(&plain, "i").state(State::Checked));
    assert!(!subject(&plain, "d").is_link());
    let state = States {
        focus: a,
        checked: i,
    };
    let tree = HtmlTree::with_state(&doc, &state).unwrap();
    let css = StyleSet::new(&[(
        Origin::Author,
        "a:hover:focus{color:red} input:checked{color:blue} a:visited{color:green!important}",
    )])
    .unwrap();
    let a = css.compute(&subject(&tree, "a"), None, 16., "").unwrap();
    assert_eq!(
        a.get(Property::Color),
        Some(&V::Color(Color {
            red: 255,
            green: 0,
            blue: 0,
            alpha: 255
        }))
    );
    let i = css.compute(&subject(&tree, "i"), None, 16., "").unwrap();
    assert_eq!(
        i.get(Property::Color),
        Some(&V::Color(Color {
            red: 0,
            green: 0,
            blue: 255,
            alpha: 255
        }))
    );
}
#[test]
fn index_budget_edges_and_dense_documents() {
    for count in [MAX_INDEX_NODES - 1, MAX_INDEX_NODES, MAX_INDEX_NODES + 1] {
        assert_eq!(
            check_size(count),
            if count > MAX_INDEX_NODES {
                Err(Error::Nodes)
            } else {
                Ok(())
            }
        );
    }
    let mut work = Work { left: 1 };
    assert_eq!(work.step(), Ok(()));
    assert_eq!(work.step(), Err(Error::Work));
    let doc = td_html::document::parse(&"<br>".repeat(MAX_INDEX_NODES - 4)).unwrap();
    assert_eq!(doc.len(), MAX_INDEX_NODES);
    let tree = HtmlTree::new(&doc).unwrap();
    assert_eq!(tree.len(), MAX_INDEX_NODES - 1);
    assert!(tree.index_bytes() <= MAX_INDEX_BYTES);
    let doc = td_html::document::parse(&format!("<div id=p>{}<i id=i>", "<!--x-->".repeat(10_000)))
        .unwrap();
    let tree = HtmlTree::new(&doc).unwrap();
    assert_eq!(
        subject(&tree, "p").first_child().unwrap().node_id(),
        subject(&tree, "i").node_id()
    );
    assert!(!subject(&tree, "p").has_text());
}

#[test]
fn allocation_refusal_releases_partial_index() {
    let document = td_html::document::parse("<main><p>one</p><p>two</p></main>").unwrap();
    let budget = document.budget();
    let baseline = budget.used();
    for attempts in 0..2 {
        let result = td_alloc::with_reservation_limit(attempts, || HtmlTree::new(&document));
        assert!(matches!(
            result,
            Err(Error::Memory(BudgetError::Allocation))
        ));
        assert_eq!(budget.used(), baseline);
    }
    let tree = td_alloc::with_reservation_limit(2, || HtmlTree::new(&document)).unwrap();
    assert_eq!(budget.used(), baseline + tree.index_bytes());
    drop(tree);
    assert_eq!(budget.used(), baseline);
}

#[test]
fn page_budget_keeps_overlapping_indexes_admitted() {
    let document = td_html::document::parse("<div><span>x</span><!--y--></div>").unwrap();
    let payload = document.len() * (std::mem::size_of::<Links>() + std::mem::size_of::<NodeId>());
    let metadata = Budget::metadata_bytes();
    let root = Budget::new(3 * metadata + 2 * payload).unwrap();
    let first_budget = root.child(metadata + payload).unwrap();
    let second_budget = root.child(metadata + 2 * payload).unwrap();
    let first = HtmlTree::with_budget(&document, &first_budget).unwrap();
    drop(first_budget);
    let second = HtmlTree::with_state_and_budget(&document, &NO_STATE, &second_budget).unwrap();
    assert_eq!(root.used(), root.limit());
    assert!(matches!(
        HtmlTree::with_budget(&document, &second_budget),
        Err(Error::Memory(BudgetError::Limit))
    ));
    drop(first);
    let replacement = HtmlTree::with_budget(&document, &second_budget).unwrap();
    assert_eq!(root.used(), 2 * metadata + 2 * payload);
    assert_eq!(
        replacement
            .elements()
            .map(|e| e.node_id())
            .collect::<Vec<_>>(),
        second.elements().map(|e| e.node_id()).collect::<Vec<_>>()
    );
    drop(second_budget);
    drop(second);
    drop(replacement);
    assert_eq!(root.used(), metadata);
}
