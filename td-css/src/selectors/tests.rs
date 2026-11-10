#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
use crate::test_tree::{tree, Handle};

fn matching(input: &str) -> Vec<usize> {
    let nodes = tree();
    let selector = SelectorList::parse(input).unwrap();
    (0..nodes.len())
        .filter(|&index| {
            selector
                .matching_specificity(&Handle {
                    nodes: &nodes,
                    index,
                })
                .unwrap()
                .is_some()
        })
        .collect()
}

#[test]
fn type_id_class_attribute_and_combinator_examples() {
    assert_eq!(matching("DIV > p.hot"), [3, 5]);
    assert_eq!(matching("#container p"), [3, 5]);
    assert_eq!(matching("p + span"), [4]);
    assert_eq!(matching("p ~ p"), [5]);
    assert_eq!(matching("body > aside p"), [8]);
    assert_eq!(matching(r"#\74 arget"), [6]);
    assert_eq!(matching("[href]"), [6]);
    assert_eq!(matching("[LANG|=en]"), [3]);
    assert_eq!(matching("[class~=hot]"), [3, 5, 8]);
    assert_eq!(matching("[data-x^=Alpha][data-x$=Beta][data-x*=ha]"), [3]);
    assert_eq!(matching("[data-x='alpha beta' i]"), [3]);
    assert!(matching("[data-x='alpha beta' s]").is_empty());
    assert!(matching("[data-x^='']").is_empty());
    assert!(matching("[data-x$='']").is_empty());
    assert!(matching("[data-x*='']").is_empty());
    assert!(matching("[class~='']").is_empty());
    assert!(matching("[class~='hot first']").is_empty());
    assert_eq!(matching("* > #target"), [6]);
    assert_eq!(matching("div, aside"), [2, 7]);
}

#[test]
fn structural_pseudos_count_elements_and_types() {
    assert_eq!(matching(":root"), [0]);
    assert_eq!(matching("div > :first-child"), [3]);
    assert_eq!(matching("div > :last-child"), [5]);
    assert_eq!(matching("p:only-child"), [8]);
    assert_eq!(matching(":root:only-child"), [0]);
    assert_eq!(matching("div > p:first-of-type"), [3]);
    assert_eq!(matching("div > p:last-of-type"), [5]);
    assert_eq!(matching("div > :only-of-type"), [4]);
    assert_eq!(matching("div > :nth-child(odd)"), [3, 5]);
    assert_eq!(matching("div > :nth-last-child(2)"), [4]);
    assert_eq!(matching("div > :nth-of-type(2)"), [5]);
    assert_eq!(matching("div > p:nth-last-of-type(2)"), [3]);
    assert_eq!(matching("div > :nth-child(-n+2)"), [3, 4]);
    assert_eq!(matching(":empty"), [3, 6, 8]);
    assert!(matching("span:empty").is_empty());
}

#[test]
fn nth_spec_grammar_preserves_sign_and_whitespace_rules() {
    // CSS Syntax §6 examples and token grammar.
    for (text, expected) in [
        ("2n+0", (2, 0)),
        ("even", (2, 0)),
        ("4n+1", (4, 1)),
        ("-1n+6", (-1, 6)),
        ("-4n+10", (-4, 10)),
        ("0n+5", (0, 5)),
        ("5", (0, 5)),
        ("n", (1, 0)),
        ("3n-6", (3, -6)),
        ("3n + 1", (3, 1)),
        ("+3n - 2", (3, -2)),
        ("-n+ 6", (-1, 6)),
        ("+6", (0, 6)),
        ("+n- 2", (1, -2)),
        ("n-2", (1, -2)),
        ("-n-2", (-1, -2)),
        ("3n- 6", (3, -6)),
        ("n/**/+1", (1, 1)),
        ("N", (1, 0)),
        ("ODD", (2, 1)),
    ] {
        let components = syntax::parse_components(text).unwrap();
        assert_eq!(nth(components.value()).unwrap(), expected, "{text}");
    }
    for text in [
        "3 n", "+ 2n", "+ 2", "3n + -6", "n 1", "n--1", "n-+1", "+-n", "2.0n", "1.0", "n of p", "",
        "n + 1 2", "n- 1.0", "+ n", "nfoo",
    ] {
        assert!(
            SelectorList::parse(&format!(":nth-child({text})")).is_err(),
            "{text}"
        );
    }
}

#[test]
fn logical_lists_specificity_and_privacy() {
    assert_eq!(matching("p:not(.first)"), [5, 8]);
    assert_eq!(matching(":is(div > .first, aside > p)"), [3, 8]);
    assert_eq!(matching("p:where(.first, .last)"), [3, 5]);
    assert_eq!(matching(":is(:unsupported, .first)"), [3]);
    assert!(matching(":is(:unsupported)").is_empty());
    assert!(matching(":where()").is_empty());
    assert_eq!(matching(":link"), [6]);
    assert!(matching(":visited").is_empty());
    assert_eq!(matching(":link:not(:visited)"), [6]);
    assert_eq!(
        matching(":not(:is(:visited, :link))"),
        [0, 1, 2, 3, 4, 5, 7, 8]
    );
    let selector = SelectorList::parse("p:is(.first, #absent)").unwrap();
    let nodes = tree();
    assert_eq!(
        selector
            .matching_specificity(&Handle {
                nodes: &nodes,
                index: 3
            })
            .unwrap(),
        Some(Specificity {
            ids: 1,
            classes: 0,
            types: 1
        })
    );
    assert_eq!(
        SelectorList::parse("#a:where(#b.x p):not(.a,p)")
            .unwrap()
            .specificities()
            .collect::<Vec<_>>(),
        [Specificity {
            ids: 1,
            classes: 1,
            types: 0
        }]
    );
    assert_eq!(
        SelectorList::parse("p, .hot, #no")
            .unwrap()
            .matching_specificity(&Handle {
                nodes: &nodes,
                index: 3
            })
            .unwrap(),
        Some(Specificity {
            ids: 0,
            classes: 1,
            types: 0
        })
    );
}

#[test]
fn invalid_selectors_do_not_get_repaired_into_other_selectors() {
    for input in [
        "",
        "p,",
        ",p",
        "p,:unknown",
        ":not()",
        ":not(p,:unknown)",
        "a/**/b",
        "#123",
        ". a",
        "p > ",
        ">p",
        "p++p",
        "p||p",
        "p|a",
        "[a~ =b]",
        "[a=]",
        "[a=1]",
        "[a=b q]",
        "::before",
        ":lang(en)",
        ":has(p)",
        ":hover",
        ":nth-child()",
        "p()",
        "*p",
        "p*",
        "p [",
        "[a|b]",
        ": is(p)",
    ] {
        assert!(SelectorList::parse(input).is_err(), "{input}");
    }
}

#[test]
fn compile_bounds_cannot_be_hidden_in_forgiving_lists() {
    assert!(SelectorList::parse(&vec!["p"; MAX_SELECTORS].join(",")).is_ok());
    assert!(matches!(
        SelectorList::parse(&vec!["p"; MAX_SELECTORS + 1].join(",")),
        Err(Error::Limit("MAX_SELECTORS"))
    ));
    assert!(matches!(
        SelectorList::parse(&format!(
            ":is({})",
            vec![":unknown"; MAX_SELECTORS].join(",")
        )),
        Err(Error::Limit("MAX_SELECTORS"))
    ));
    assert!(SelectorList::parse(&".a".repeat(MAX_SELECTOR_PARTS)).is_ok());
    assert!(matches!(
        SelectorList::parse(&".a".repeat(MAX_SELECTOR_PARTS + 1)),
        Err(Error::Limit("MAX_SELECTOR_PARTS"))
    ));
    assert!(SelectorList::parse(&vec!["p"; MAX_COMPOUNDS].join(" ")).is_ok());
    assert!(matches!(
        SelectorList::parse(&vec!["p"; MAX_COMPOUNDS + 1].join(" ")),
        Err(Error::Limit("MAX_COMPOUNDS"))
    ));
    let nested = |count| format!("{}p{}", ":is(".repeat(count), ")".repeat(count));
    assert!(SelectorList::parse(&nested(MAX_SELECTOR_DEPTH)).is_ok());
    assert!(matches!(
        SelectorList::parse(&nested(MAX_SELECTOR_DEPTH + 1)),
        Err(Error::Limit("MAX_SELECTOR_DEPTH"))
    ));
    assert!(SelectorList::parse(":nth-child(1000000000n-1000000000)").is_ok());
    assert!(matches!(
        SelectorList::parse(":nth-child(1000000001n)"),
        Err(Error::Limit("MAX_NTH_COEFFICIENT"))
    ));
}

#[test]
fn matching_budgets_bound_cycles_and_large_attributes() {
    let mut nodes = tree();
    nodes[0].parent = Some(0);
    nodes[0].previous = Some(0);
    nodes[0].next = Some(0);
    let root = Handle {
        nodes: &nodes,
        index: 0,
    };
    for input in [
        ".missing html",
        ".missing ~ html",
        ":nth-child(1)",
        ":nth-last-child(1)",
    ] {
        assert_eq!(
            SelectorList::parse(input)
                .unwrap()
                .matching_specificity(&root),
            Err(Error::Limit("MAX_MATCH_STEPS")),
            "{input}"
        );
    }
    nodes[3].attrs.push(("huge", "x".repeat(MAX_ELEMENT_BYTES)));
    let node = Handle {
        nodes: &nodes,
        index: 3,
    };
    assert!(SelectorList::parse("[huge]")
        .unwrap()
        .matching_specificity(&node)
        .unwrap()
        .is_some());
    assert_eq!(
        SelectorList::parse(&"[huge]".repeat(MAX_MATCH_STEPS / MAX_ELEMENT_BYTES + 1))
            .unwrap()
            .matching_specificity(&node),
        Err(Error::Limit("MAX_MATCH_STEPS"))
    );
    nodes[3].attrs.last_mut().unwrap().1.push('x');
    let node = Handle {
        nodes: &nodes,
        index: 3,
    };
    assert_eq!(
        SelectorList::parse("[huge]")
            .unwrap()
            .matching_specificity(&node),
        Err(Error::Limit("MAX_ELEMENT_BYTES"))
    );
    let mut budget = MatchBudget::new();
    assert!(budget.charge(MAX_MATCH_STEPS).is_ok());
    assert_eq!(budget.step(), Err(Error::Limit("MAX_MATCH_STEPS")));
}

#[test]
fn backtracking_keeps_searching_after_an_ancestor_candidate_fails() {
    let mut nodes = tree();
    nodes[2].tag = "body";
    assert_eq!(
        SelectorList::parse("html > body a")
            .unwrap()
            .matching_specificity(&Handle {
                nodes: &nodes,
                index: 6
            })
            .unwrap(),
        Some(Specificity {
            types: 3,
            ..Specificity::default()
        })
    );
}

#[test]
fn deterministic_selector_inputs_terminate() {
    let alphabet = [
        "p", ".", "#x", ":", "not(", "is(", ")", "[a]", " ", ">", "+", "~", ",", "*", "0", "é",
    ];
    let mut state = 123456789u32;
    let nodes = tree();
    for _ in 0..1000 {
        let mut input = String::new();
        for _ in 0..32 {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            input.push_str(alphabet[state as usize % alphabet.len()]);
        }
        if let Ok(selector) = SelectorList::parse(&input) {
            for index in 0..nodes.len() {
                let _ = selector.matching_specificity(&Handle {
                    nodes: &nodes,
                    index,
                });
            }
        }
    }
}

#[test]
fn combined_logical_and_compound_depth_fits_the_test_thread_stack() {
    let mut selector = "html".to_owned();
    for _ in 0..MAX_SELECTOR_DEPTH {
        selector = format!("{}:is({selector})", "html > ".repeat(60));
    }
    let selector = SelectorList::parse(&selector).unwrap();
    let mut nodes = tree();
    nodes[0].parent = Some(0);
    assert!(selector
        .matching_specificity(&Handle {
            nodes: &nodes,
            index: 0
        })
        .unwrap()
        .is_some());
}

#[test]
fn attribute_flags_fold_ascii_only_and_classes_use_html_space() {
    let mut nodes = tree();
    nodes[3].attrs.push(("label", "ÄBC".into()));
    nodes[3].attrs.push(("empty", String::new()));
    let node = Handle {
        nodes: &nodes,
        index: 3,
    };
    for (selector, expected) in [
        ("[label='Äbc' i]", true),
        ("[label='äbc' i]", false),
        ("[empty='']", true),
        ("[empty~='']", false),
    ] {
        assert_eq!(
            SelectorList::parse(selector)
                .unwrap()
                .matching_specificity(&node)
                .unwrap()
                .is_some(),
            expected
        );
    }
    nodes[3]
        .attrs
        .iter_mut()
        .find(|(name, _)| *name == "class")
        .unwrap()
        .1 = "hot\x0bfirst".into();
    assert!(SelectorList::parse(".hot")
        .unwrap()
        .matching_specificity(&Handle {
            nodes: &nodes,
            index: 3
        })
        .unwrap()
        .is_none());
}

#[test]
fn forgiving_lists_first_require_valid_any_value_syntax() {
    // Selectors §16.1 and Syntax §8.2: recovery starts after <any-value>.
    for selector in [
        ":is(p, ])",
        ":where(p, url(a b))",
        ":is(p, \"bad\n)",
        ":is(p, :unknown(url(a b)))",
        ":is(p, [x=bad) ])",
        ":is(p, :where(a, ]))",
    ] {
        assert!(
            matches!(SelectorList::parse(selector), Err(Error::InvalidSelector)),
            "{selector}"
        );
    }
    assert_eq!(matching(":is(p, :unknown(url(a)))"), [3, 5, 8]);
    assert_eq!(matching(":where(p, !; :unknown)"), [3, 5, 8]);
}
