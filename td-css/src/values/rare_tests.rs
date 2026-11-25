//! The rare-property group: shared by default, copied when set
//! (DESIGN.md, "Computed-style sharing").
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
// Wrapped so the allocation guard reads it as test code.
#[cfg(test)]
mod cases {
    use super::super::*;
    use crate::test_tree::{tree, Handle};
    use crate::{Origin, StyleSet};
    use td_alloc::BudgetShared;
    use Property::*;

    /// The test tree's element `index` under `css`, its parent's style given.
    fn at(
        set: &StyleSet,
        index: usize,
        parent: Option<&ComputedStyle>,
        inline: &str,
    ) -> ComputedStyle {
        let nodes = tree();
        set.compute(
            &Handle {
                nodes: &nodes,
                index,
            },
            parent,
            16.0,
            inline,
        )
        .unwrap()
    }
    fn shared(a: &ComputedStyle, b: &ComputedStyle) -> bool {
        BudgetShared::ptr_eq(&a.rare, &b.rare)
    }
    fn number(style: &ComputedStyle, p: Property) -> f64 {
        match style.get(p) {
            Some(ComputedValue::Number(n)) => *n,
            other => panic!("{p:?}: {other:?}"),
        }
    }
    fn length(style: &ComputedStyle, p: Property) -> Length {
        match style.get(p) {
            Some(ComputedValue::Length(l)) => *l,
            other => panic!("{p:?}: {other:?}"),
        }
    }
    fn word(style: &ComputedStyle, p: Property) -> &'static str {
        match style.get(p) {
            Some(ComputedValue::Keyword(k)) => k,
            other => panic!("{p:?}: {other:?}"),
        }
    }

    #[test]
    fn the_group_is_the_roster_tail_none_inherited_each_with_a_plain_seed() {
        let rare = Property::rare_roster();
        assert_eq!(rare.len(), RARE);
        assert_eq!(Property::common_roster().len() + RARE, Property::ALL.len());
        assert_eq!(rare.first(), Some(&Property::FIRST_RARE));
        for (slot, p) in rare.iter().enumerate() {
            assert_eq!(p.rare(), Some(slot));
            assert!(!p.inherited(), "{p:?}");
            assert!(p.seed().is_some(), "{p:?}");
        }
        for p in Property::common_roster() {
            assert_eq!(p.rare(), None);
        }
        for (i, p) in Property::ALL.iter().enumerate() {
            assert_eq!(*p as usize, i);
        }
        let memory = crate::memory().unwrap();
        let fresh = ComputedStyle::new(&memory).unwrap();
        assert_eq!(*fresh.rare, Rare::initial().unwrap());
        for p in rare {
            assert_eq!(fresh.get(*p), Some(&p.initial(&memory).unwrap()));
        }
    }

    #[test]
    fn a_rare_longhand_set_on_one_element_leaves_its_siblings_sharing_the_default() {
        let set = StyleSet::new(&[(Origin::Author, ".first{order:2}")]).unwrap();
        let parent = at(&set, 2, None, "");
        // Elements 3, 4 and 5 are siblings; only 3 is `.first`.
        let first = at(&set, 3, Some(&parent), "");
        let second = at(&set, 4, Some(&parent), "");
        let third = at(&set, 5, Some(&parent), "");
        assert_eq!(number(&first, Order), 2.0);
        assert_eq!(number(&second, Order), 0.0);
        assert!(shared(&parent, &second));
        assert!(shared(&second, &third));
        assert!(!shared(&first, &second));
        // Its children do not inherit it: they share the default again.
        let child = at(&set, 6, Some(&first), "");
        assert_eq!(number(&child, Order), 0.0);
        assert!(shared(&child, &second));
        // A declaration that computes to the initial value copies nothing.
        let initial = at(
            &set,
            4,
            Some(&parent),
            "order:0;flex-shrink:1;top:auto;clear:none",
        );
        assert!(shared(&initial, &second));
        // Common declarations never touch the group.
        let common = at(&set, 4, Some(&parent), "width:3px;color:red;float:left");
        assert!(shared(&common, &second));
    }

    #[test]
    fn writing_a_rare_value_copies_the_group_and_leaves_its_sharers() {
        let set = StyleSet::new(&[]).unwrap();
        let a = at(&set, 3, None, "");
        let mut b = a.try_clone().unwrap();
        assert!(shared(&a, &b));
        b.set(Width, ComputedValue::Length(Length::Px(3.0)))
            .unwrap();
        assert!(shared(&a, &b));
        b.set(Order, ComputedValue::Number(5.0)).unwrap();
        assert!(!shared(&a, &b));
        assert_eq!(number(&a, Order), 0.0);
        assert_eq!(number(&b, Order), 5.0);
        // The copy keeps every other value.
        for p in Property::rare_roster().iter().filter(|p| **p != Order) {
            assert_eq!(a.get(*p), b.get(*p), "{p:?}");
        }
        assert_ne!(a, b);
        let mut c = a.try_clone().unwrap();
        c.set(Order, ComputedValue::Number(5.0)).unwrap();
        c.set(Width, ComputedValue::Length(Length::Px(3.0)))
            .unwrap();
        // Equal by contents, though each wrote its own copy.
        assert!(!shared(&b, &c));
        assert_eq!(b, c);
    }

    #[test]
    fn inherit_initial_unset_and_var_resolve_rare_longhands() {
        let set = StyleSet::new(&[]).unwrap();
        let parent = at(
            &set,
            2,
            None,
            "order:3;opacity:.5;top:4px;flex-basis:2em;clear:both;font-size:10px;--n:7",
        );
        let child = at(
            &set,
            3,
            Some(&parent),
            "order:inherit;opacity:initial;top:unset;flex-basis:inherit;clear:inherit;\
             flex-grow:var(--n);flex-shrink:var(--missing);column-gap:var(--gap, 1em);\
             table-layout:var(--missing, fixed)",
        );
        assert_eq!(number(&child, Order), 3.0);
        assert_eq!(number(&child, Opacity), 1.0);
        assert_eq!(length(&child, Top), Length::Auto);
        // The parent's computed px, not its em against the child's size.
        assert_eq!(length(&child, FlexBasis), Length::Px(20.0));
        assert_eq!(word(&child, Clear), "both");
        assert_eq!(number(&child, FlexGrow), 7.0);
        assert_eq!(number(&child, FlexShrink), 1.0);
        assert_eq!(length(&child, ColumnGap), Length::Px(10.0));
        assert_eq!(word(&child, TableLayout), "fixed");
        // Initial, unset and an invalid var() all leave the default group.
        let reset = at(
            &set,
            3,
            Some(&parent),
            "order:initial;opacity:unset;top:var(--missing);clear:initial",
        );
        assert!(shared(&reset, &at(&set, 4, Some(&parent), "")));
        // Without a parent, inherit is the initial value.
        let root = at(&set, 3, None, "order:inherit;clear:inherit");
        assert_eq!(number(&root, Order), 0.0);
        assert_eq!(word(&root, Clear), "none");
    }

    #[test]
    fn shorthands_expand_into_rare_longhands() {
        let set = StyleSet::new(&[]).unwrap();
        let style = at(
            &set,
            3,
            None,
            "flex:2 3 10px;gap:1px 2px;flex-flow:column wrap",
        );
        assert_eq!(number(&style, FlexGrow), 2.0);
        assert_eq!(number(&style, FlexShrink), 3.0);
        assert_eq!(length(&style, FlexBasis), Length::Px(10.0));
        assert_eq!(length(&style, RowGap), Length::Px(1.0));
        assert_eq!(length(&style, ColumnGap), Length::Px(2.0));
        assert_eq!(word(&style, FlexDirection), "column");
        assert_eq!(word(&style, FlexWrap), "wrap");
        let substituted = at(&set, 3, None, "--f:4;flex:var(--f);gap:var(--g, 5px)");
        assert_eq!(number(&substituted, FlexGrow), 4.0);
        assert_eq!(number(&substituted, FlexShrink), 1.0);
        assert_eq!(length(&substituted, FlexBasis), Length::Percent(0.0));
        assert_eq!(length(&substituted, RowGap), Length::Px(5.0));
        assert_eq!(length(&substituted, ColumnGap), Length::Px(5.0));
        // A shorthand's reset of an omitted longhand leaves it initial.
        let flow = at(&set, 3, None, "flex-wrap:wrap;flex-flow:column");
        assert_eq!(word(&flow, FlexWrap), "nowrap");
    }

    #[test]
    fn important_rare_declarations_win_as_any_do() {
        let css = "p{order:1 !important;flex-grow:9 !important;opacity:.2}";
        let set = StyleSet::new(&[(Origin::Author, css)]).unwrap();
        let style = at(&set, 3, None, "order:2;flex-grow:3 !important;opacity:.4");
        assert_eq!(number(&style, Order), 1.0);
        assert_eq!(number(&style, FlexGrow), 3.0);
        assert_eq!(number(&style, Opacity), 0.4);
        let ua = "p{clear:left !important}";
        let set = StyleSet::new(&[(Origin::UserAgent, ua)]).unwrap();
        let style = at(&set, 3, None, "clear:right !important");
        assert_eq!(word(&style, Clear), "left");
    }

    #[test]
    fn a_negative_zero_is_kept_to_the_bit() {
        let set = StyleSet::new(&[]).unwrap();
        let initial = at(&set, 3, None, "");
        let negative = at(&set, 3, None, "order:-0;flex-grow:-0");
        // Equal under `==`, but not the initial value's bits: a copy.
        assert!(!shared(&negative, &initial));
        assert!(number(&negative, Order).is_sign_negative());
        assert!(number(&negative, FlexGrow).is_sign_negative());
        let positive = at(&set, 3, None, "order:0;flex-grow:+0");
        assert!(shared(&positive, &initial));
    }
}
