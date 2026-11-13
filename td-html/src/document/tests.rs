#![cfg(test)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::collections::BTreeSet;
fn shape(d: &Document, id: NodeId, out: &mut String) {
    let n = d.node(id).unwrap();
    match n.kind() {
        NodeKind::Element(e) => {
            out.push('<');
            if e.namespace == Namespace::Svg {
                out.push_str("svg:");
            }
            if e.namespace == Namespace::MathMl {
                out.push_str("math:");
            }
            out.push_str(&e.name);
            out.push('>');
            if let Some(f) = e.template_contents {
                out.push_str("<content>");
                for (id, _) in d.children(f) {
                    shape(d, id, out);
                }
                out.push_str("</content>");
            }
            for (id, _) in d.children(id) {
                shape(d, id, out);
            }
            out.push_str("</");
            if e.namespace == Namespace::Svg {
                out.push_str("svg:");
            }
            if e.namespace == Namespace::MathMl {
                out.push_str("math:");
            }
            out.push_str(&e.name);
            out.push('>');
        }
        NodeKind::Text(s) => out.push_str(s),
        NodeKind::Comment(s) => {
            out.push_str("<!--");
            out.push_str(s);
            out.push_str("-->");
        }
        NodeKind::Doctype(_) => {}
        _ => {
            for (id, _) in d.children(id) {
                shape(d, id, out);
            }
        }
    }
}
fn check(input: &str, expected: &str) {
    let d = parse(input).unwrap();
    invariants(&d);
    let mut out = String::new();
    shape(&d, d.root(), &mut out);
    assert_eq!(out, expected, "{input:?}");
}
fn body(input: &str, expected: &str) {
    check(
        input,
        &format!("<html><head></head><body>{expected}</body></html>"),
    );
}
fn invariants(d: &Document) {
    assert_eq!(d.node(d.root()).unwrap().parent(), None);
    let mut seen = BTreeSet::new();
    let roots = d
        .nodes()
        .filter(|(_, n)| n.parent().is_none())
        .map(|(id, _)| id)
        .collect::<Vec<_>>();
    let mut todo = roots;
    while let Some(parent) = todo.pop() {
        assert!(seen.insert(parent));
        let children = d.children(parent).map(|(id, _)| id).collect::<Vec<_>>();
        let n = d.node(parent).unwrap();
        assert_eq!(n.first_child(), children.first().copied());
        assert_eq!(n.last_child(), children.last().copied());
        for (i, &id) in children.iter().enumerate() {
            let c = d.node(id).unwrap();
            assert_eq!(c.parent(), Some(parent));
            assert_eq!(
                c.previous_sibling(),
                i.checked_sub(1).and_then(|i| children.get(i)).copied()
            );
            assert_eq!(c.next_sibling(), children.get(i + 1).copied());
            todo.push(id);
        }
    }
    assert_eq!(seen.len(), d.len());
    for (id, n) in d.nodes() {
        let mut next = Some(id);
        let mut depth = 0;
        while let Some(id) = next {
            next = d.node(id).unwrap().parent();
            if next.is_some() {
                depth += 1;
            }
            assert!(depth <= html5::MAX_DEPTH);
        }
        if let Some(e) = n.element() {
            if let Some(f) = e.template_contents {
                assert!(matches!(
                    d.node(f).unwrap().kind(),
                    NodeKind::DocumentFragment
                ));
                assert_eq!(d.node(f).unwrap().parent(), None);
                assert_eq!(n.first_child(), None);
            }
        }
    }
}
// Original fixtures based on html5lib-tests tests1.dat/tests2.dat and
// WHATWG initial, before-html, before-head, in-head and after-head modes.
#[test]
fn document_head_and_text_modes() {
    body("", "");
    body("  <body>x", "x");
    body("<html><head></head><body>x", "x");
    check(
        "<!--a--><title>x&amp;y</title><p>z",
        "<!--a--><html><head><title>x&y</title></head><body><p>z</p></body></html>",
    );
    check(
        "<head><noscript><style>x<b></style></noscript></head>x",
        "<html><head><noscript><style>x<b></style></noscript></head><body>x</body></html>",
    );
    check(
        "<head><noscript><p>x",
        "<html><head><noscript></noscript></head><body><p>x</p></body></html>",
    );
    check(
        "<head></head><title>x</title><body>y",
        "<html><head><title>x</title></head><body>y</body></html>",
    );
    body(
        "<textarea>\nx&amp;<b></textarea><p>y",
        "<textarea>x&<b></textarea><p>y</p>",
    );
    body("<pre>\n\nx</pre>", "<pre>\nx</pre>");
    body("<plaintext>x<b>&amp;", "<plaintext>x<b>&amp;</plaintext>");
    check(
        "<script><!--><script></script><p>x",
        "<html><head><script><!--><script></script></head><body><p>x</p></body></html>",
    );
}
// tests1.dat, tests3.dat, adoption01.dat and adoption02.dat; WHATWG in-body,
// implied end tags, active-formatting reconstruction and adoption agency.
#[test]
fn body_and_formatting_repairs() {
    for (input, expected) in [
        ("<p>a<p>b", "<p>a</p><p>b</p>"),
        ("<ul><li>a<li>b</ul>", "<ul><li>a</li><li>b</li></ul>"),
        (
            "<dl><dt>a<dd>b<dt>c",
            "<dl><dt>a</dt><dd>b</dd><dt>c</dt></dl>",
        ),
        ("<h1>a<h2>b</h1>c", "<h1>a</h1><h2>b</h2>c"),
        ("<p>a<div>b</div>c", "<p>a</p><div>b</div>c"),
        ("<body></p>x", "<p></p>x"),
        ("<div/>x", "<div>x</div>"),
        ("<form><div></form>x</div>", "<form><div>x</div></form>"),
        ("<b><i>x</b>y</i>", "<b><i>x</i></b><i>y</i>"),
        ("<b><p>x</b>y", "<b></b><p><b>x</b>y</p>"),
        ("<a>x<div>y</a>z", "<a>x</a><div><a>y</a>z</div>"),
        ("<a>x<a>y</a>z", "<a>x</a><a>y</a>z"),
        ("<p><b>x</p>y", "<p><b>x</b></p><b>y</b>"),
        ("<button>x<button>y", "<button>x</button><button>y</button>"),
        ("<ruby>a<rt>b<rt>c", "<ruby>a<rt>b</rt><rt>c</rt></ruby>"),
    ] {
        body(input, expected);
    }
}
// tables01.dat, tests1.dat and foster-parenting examples in WHATWG's table,
// table-text, caption, column-group, table-body, row and cell modes.
#[test]
fn table_modes_and_foster_parenting() {
    for (input, expected) in [
        (
            "<table><td>x<td>y",
            "<table><tbody><tr><td>x</td><td>y</td></tr></tbody></table>",
        ),
        (
            "<table> a <tr><td>b</table>",
            " a <table><tbody><tr><td>b</td></tr></tbody></table>",
        ),
        (
            "<table> \n<tr><td>x",
            "<table> \n<tbody><tr><td>x</td></tr></tbody></table>",
        ),
        (
            "<table><caption>x<tr><td>y",
            "<table><caption>x</caption><tbody><tr><td>y</td></tr></tbody></table>",
        ),
        (
            "<table><col><tr><th>x",
            "<table><colgroup><col></col></colgroup><tbody><tr><th>x</th></tr></tbody></table>",
        ),
        (
            "<table><tbody><tr><td>x</tbody><tfoot><tr><td>y",
            "<table><tbody><tr><td>x</td></tr></tbody><tfoot><tr><td>y</td></tr></tfoot></table>",
        ),
        (
            "<table><form><input type=hidden><tr><td>x",
            "<table><form></form><input></input><tbody><tr><td>x</td></tr></tbody></table>",
        ),
        (
            "<table><b>x<tr><td>y</table>z",
            "<b>x</b><table><tbody><tr><td>y</td></tr></tbody></table><b>z</b>",
        ),
        (
            "<table><tr><td><b>x</td><td>y",
            "<table><tbody><tr><td><b>x</b></td><td>y</td></tr></tbody></table>",
        ),
    ] {
        body(input, expected);
    }
}
// template.dat; WHATWG in-template, template insertion-mode stack,
// detached contents and the adjusted insertion location.
#[test]
fn templates_have_detached_content_and_restore_modes() {
    check("<template><tr><td>x</template><p>y","<html><head><template><content><tr><td>x</td></tr></content></template></head><body><p>y</p></body></html>");
    check("<template> <tr><td>x</template>","<html><head><template><content> <tr><td>x</td></tr></content></template></head><body></body></html>");
    body("<body><template><!--a--><template><p>x</template>y</template>z","<template><content><!--a--><template><content><p>x</p></content></template>y</content></template>z");
    body(
        "<body><template><div>x",
        "<template><content><div>x</div></content></template>",
    );
}
// Historical select modes are deliberately retained; newer customizable
// select fixtures which admit arbitrary elements/selectedcontent are excluded.
#[test]
fn legacy_select_and_after_body_modes() {
    body(
        "<select><option>x<option>y",
        "<select><option>x</option><option>y</option></select>",
    );
    body("<select><optgroup><option>x<optgroup><option>y","<select><optgroup><option>x</option></optgroup><optgroup><option>y</option></optgroup></select>");
    body("<select><input>x", "<select></select><input></input>x");
    body(
        "<table><tr><td><select><option>x</table>y",
        "<table><tbody><tr><td><select><option>x</option></select></td></tr></tbody></table>y",
    );
    check(
        "<body>x</body><!--a--></html><!--b-->",
        "<html><head></head><body>x</body><!--a--></html><!--b-->",
    );
    body("<body>x</html><p>y", "x<p>y</p>");
}
// tests9.dat, tests10.dat and foreign-fragment-independent webkit02.dat:
// WHATWG foreign dispatcher, integration points, adjustments and CDATA.
#[test]
fn foreign_content_is_namespaced_and_bounded() {
    body(
        "<svg><g><![CDATA[a<b]]></g></svg>",
        "<svg:svg><svg:g>a<b</svg:g></svg:svg>",
    );
    body(
        "<svg><foreignObject><p>x</p></foreignObject></svg>",
        "<svg:svg><svg:foreignObject><p>x</p></svg:foreignObject></svg:svg>",
    );
    body(
        "<math><mi><b>x</b></mi></math>",
        "<math:math><math:mi><b>x</b></math:mi></math:math>",
    );
    body("<svg><g><p>x", "<svg:svg><svg:g></svg:g></svg:svg><p>x</p>");
    body("<div><svg><path><foreignObject><math></div>x","<div><svg:svg><svg:path><svg:foreignObject><math:math>x</math:math></svg:foreignObject></svg:path></svg:svg></div>");
    let d=parse("<svg viewbox='x' xml:lang='en' xml:base='u' xlink:href='v'><lineargradient/></svg><math definitionurl='z'>").unwrap();
    invariants(&d);
    let svg = d
        .nodes()
        .find_map(|(_, n)| n.element().filter(|e| e.name == "svg"))
        .unwrap();
    assert_eq!(svg.attribute("viewBox"), Some("x"));
    assert_eq!(svg.attribute("xml:base"), Some("u"));
    assert!(svg
        .attributes
        .iter()
        .any(|a| a.name == "lang" && a.namespace == AttributeNamespace::Xml));
    assert!(svg
        .attributes
        .iter()
        .any(|a| a.name == "href" && a.namespace == AttributeNamespace::XLink));
    assert!(d
        .nodes()
        .any(|(_, n)| n.element().is_some_and(|e| e.name == "linearGradient")));
}
#[test]
fn doctype_quirks_and_root_attributes() {
    for (input, mode) in [
        ("", QuirksMode::Quirks),
        ("<!doctype html>", QuirksMode::NoQuirks),
        ("<!doctype other>", QuirksMode::Quirks),
        (
            "<!doctype html PUBLIC '-//W3C//DTD HTML 4.01 Transitional//EN'>",
            QuirksMode::Quirks,
        ),
        (
            "<!doctype html PUBLIC '-//W3C//DTD HTML 4.01 Transitional//EN' ''>",
            QuirksMode::LimitedQuirks,
        ),
        (
            "<!doctype html PUBLIC '-//W3C//DTD XHTML 1.0 Transitional//EN'>",
            QuirksMode::LimitedQuirks,
        ),
    ] {
        assert_eq!(parse(input).unwrap().quirks_mode(), mode, "{input}");
    }
    let d = parse("<html id=a><body class=b><html id=c lang=en><body id=d>").unwrap();
    let html = d
        .nodes()
        .find_map(|(_, n)| n.element().filter(|e| e.name == "html"))
        .unwrap();
    assert_eq!(html.attribute("id"), Some("a"));
    assert_eq!(html.attribute("lang"), Some("en"));
    let body = d
        .nodes()
        .find_map(|(_, n)| n.element().filter(|e| e.name == "body"))
        .unwrap();
    assert_eq!(body.attribute("class"), Some("b"));
    assert_eq!(body.attribute("id"), Some("d"));
}
#[test]
fn document_limit_edges_and_allocation_model() {
    assert!(std::mem::size_of::<Node>() * 2 <= html5::NODE_CHARGE);
    assert!(std::mem::size_of::<Attribute>() * 2 <= html5::ATTRIBUTE_CHARGE);
    for nodes in [3, 4, 5] {
        assert_eq!(
            parse_with_limits(
                "",
                ParseLimits {
                    nodes,
                    ..Default::default()
                }
            )
            .map(|_| ()),
            if nodes < 4 {
                Err(ParseError::Nodes)
            } else {
                Ok(())
            }
        );
    }
    for attributes in [1, 2, 3] {
        assert_eq!(
            parse_with_limits(
                "<div a=b>",
                ParseLimits {
                    attributes,
                    ..Default::default()
                }
            )
            .map(|_| ()),
            if attributes < 2 {
                Err(ParseError::Attributes)
            } else {
                Ok(())
            }
        );
    }
    for depth in [2, 3, 4] {
        assert_eq!(
            parse_with_limits(
                "<div>",
                ParseLimits {
                    depth,
                    ..Default::default()
                }
            )
            .map(|_| ()),
            if depth < 3 {
                Err(ParseError::Depth)
            } else {
                Ok(())
            }
        );
    }
    let cost = 4 * html5::NODE_CHARGE + 24 * html5::BYTE_CHARGE;
    assert_eq!(parse("").unwrap().charged_bytes(), cost);
    for bytes in [cost - 1, cost, cost + 1] {
        assert_eq!(
            parse_with_limits(
                "",
                ParseLimits {
                    bytes,
                    ..Default::default()
                }
            )
            .map(|_| ()),
            if bytes < cost {
                Err(ParseError::Bytes)
            } else {
                Ok(())
            }
        );
    }
    let work = parse("<table>x<tr><td><b>y</table>z").unwrap().work();
    for limit in [work - 1, work, work + 1] {
        assert_eq!(
            parse_with_limits(
                "<table>x<tr><td><b>y</table>z",
                ParseLimits {
                    work: limit,
                    ..Default::default()
                }
            )
            .map(|_| ()),
            if limit < work {
                Err(ParseError::Work)
            } else {
                Ok(())
            }
        );
    }
    let d = parse(&"<div>".repeat(html5::MAX_DEPTH - 2)).unwrap();
    invariants(&d);
    assert!(matches!(
        parse(&"<div>".repeat(html5::MAX_DEPTH - 1)),
        Err(ParseError::Depth)
    ));
    let d = parse(&"<br>".repeat(html5::MAX_NODES - 4)).unwrap();
    assert_eq!(d.len(), html5::MAX_NODES);
    drop(d);
    assert!(matches!(
        parse(&"<br>".repeat(html5::MAX_NODES - 3)),
        Err(ParseError::Nodes)
    ));
}
#[test]
fn formatting_algorithm_fixed_edges() {
    for count in [2, 3, 4] {
        let d = parse(&format!("<p>{}x</p>y", "<b>".repeat(count))).unwrap();
        invariants(&d);
        assert_eq!(
            d.nodes()
                .filter(|(_, n)| n.element().is_some_and(|e| e.name == "b"))
                .count(),
            count + count.min(html5::MAX_FORMATTING_DUPLICATES)
        );
    }
    for count in [7, 8, 9] {
        let d = parse(&format!("<b>{}x</b>", "<div>".repeat(count))).unwrap();
        invariants(&d);
        assert_eq!(
            d.nodes()
                .filter(|(_, n)| n.element().is_some_and(|e| e.name == "b"))
                .count(),
            1 + count.min(html5::MAX_ADOPTION_PASSES)
        );
    }
}
#[test]
fn tree_moves_check_descendants_and_cycles_before_link_changes() {
    let mut b = builder::Builder::new(
        "",
        ParseLimits {
            depth: 3,
            ..Default::default()
        },
        &td_alloc::Budget::new(html5::MAX_TREE_BYTES).unwrap(),
    )
    .unwrap();
    let root = b.doc.root;
    let a = b.insert_named("a").unwrap();
    let child = b.insert_named("b").unwrap();
    let leaf = b.insert_named("c").unwrap();
    b.open.clear();
    let other = b.insert_named("d").unwrap();
    assert_eq!(b.attach(a, other, None), Err(ParseError::Depth));
    assert_eq!(b.node(a).unwrap().parent(), Some(root));
    assert_eq!(b.node(leaf).unwrap().parent(), Some(child));
    assert_eq!(b.attach(a, child, None), Err(ParseError::State));
    invariants(&b.doc);
}
#[test]
fn hostile_generated_recovery_keeps_link_invariants() {
    let pieces = [
        "<b>",
        "</b>",
        "<i>",
        "</i>",
        "<a>",
        "</a>",
        "<p>",
        "</p>",
        "<div>",
        "</div>",
        "<table>",
        "</table>",
        "<tr>",
        "<td>",
        "</td>",
        "<template>",
        "</template>",
        "<svg>",
        "</svg>",
        "<foreignObject>",
        "<math>",
        "</math>",
        "<mi>",
        "<select>",
        "</select>",
        "<option>",
        "x",
        " ",
        "<!--x-->",
    ];
    let mut seed = 0x1973ab4du64;
    for _ in 0..2000 {
        let mut source = String::new();
        for _ in 0..80 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            source.push_str(pieces[seed as usize % pieces.len()]);
        }
        match parse(&source) {
            Ok(d) => invariants(&d),
            Err(e) => assert!(
                matches!(
                    e,
                    ParseError::Depth
                        | ParseError::Work
                        | ParseError::Bytes
                        | ParseError::Nodes
                        | ParseError::Attributes
                ),
                "{source}: {e:?}"
            ),
        }
    }
}

#[test]
fn merged_root_attributes_keep_the_per_element_bound() {
    for limit in [2, 3, 4] {
        let r = parse_with_limits(
            "<html a><html b><html c>",
            ParseLimits {
                attributes_per_tag: limit,
                ..Default::default()
            },
        );
        assert_eq!(
            r.map(|_| ()),
            if limit < 3 {
                Err(ParseError::AttributesPerTag)
            } else {
                Ok(())
            }
        );
    }
}

#[test]
fn global_budget_ceiling_edges_and_clamping() {
    let limits = ParseLimits {
        input: usize::MAX,
        nodes: usize::MAX,
        attributes: usize::MAX,
        bytes: usize::MAX,
        depth: usize::MAX,
        attributes_per_tag: usize::MAX,
        name_bytes: usize::MAX,
        attribute_bytes: usize::MAX,
        work: usize::MAX,
    }
    .clamped();
    assert_eq!(
        (
            limits.input,
            limits.nodes,
            limits.attributes,
            limits.bytes,
            limits.depth,
            limits.attributes_per_tag,
            limits.name_bytes,
            limits.attribute_bytes,
            limits.work
        ),
        (
            html5::MAX_INPUT_BYTES,
            html5::MAX_NODES,
            html5::MAX_ATTRIBUTES,
            html5::MAX_TREE_BYTES,
            html5::MAX_DEPTH,
            html5::MAX_ATTRIBUTES_PER_TAG,
            html5::MAX_NAME_BYTES,
            html5::MAX_ATTRIBUTE_BYTES,
            html5::MAX_WORK
        )
    );
    let mut budget = html5::Budget::new(
        limits,
        &td_alloc::Budget::new(html5::MAX_TREE_BYTES).unwrap(),
    )
    .unwrap();
    budget.attributes = html5::MAX_ATTRIBUTES - 1;
    assert_eq!(budget.attribute(), Ok(()));
    assert_eq!(budget.attribute(), Err(ParseError::Attributes));
    budget.work = html5::MAX_WORK - 1;
    assert_eq!(budget.step(), Ok(()));
    assert_eq!(budget.step(), Err(ParseError::Work));
    budget.bytes = html5::MAX_TREE_BYTES - 1;
    assert_eq!(budget.charge(1), Ok(()));
    assert_eq!(budget.charge(1), Err(ParseError::Bytes));
    for count in [
        html5::MAX_ATTRIBUTES_PER_TAG - 1,
        html5::MAX_ATTRIBUTES_PER_TAG,
        html5::MAX_ATTRIBUTES_PER_TAG + 1,
    ] {
        assert_eq!(
            parse(&format!("<a {}>", "x ".repeat(count))).map(|_| ()),
            if count > html5::MAX_ATTRIBUTES_PER_TAG {
                Err(ParseError::AttributesPerTag)
            } else {
                Ok(())
            }
        );
    }
    let d = parse("").unwrap();
    assert!(d.node(NodeId::new(d.len()).unwrap()).is_none());
}

#[test]
fn integration_point_detection_does_not_rescan_attributes_per_character() {
    for encoding in ["", " encoding='invalid'", " encoding='text/html'"] {
        let count = if encoding.is_empty() { 256 } else { 255 };
        let attrs = (0..count).map(|i| format!(" a{i}=x")).collect::<String>();
        let source = format!(
            "<math><annotation-xml{attrs}{encoding}>{}",
            "x".repeat(10000)
        );
        ATTRIBUTE_VISITS.with(|v| v.set(0));
        let d = parse(&source).unwrap();
        let visits = ATTRIBUTE_VISITS.with(|v| v.get());
        assert!(
            visits <= d.work(),
            "attribute visits {visits} exceed total charged work {}",
            d.work()
        );
        invariants(&d);
    }
}

#[test]
fn allocation_refusals_reclaim_the_whole_shared_parse_budget() {
    use td_alloc::{with_reservation_limit, Budget};
    let input = "<!doctype html PUBLIC 'a' 'b'><body x=y><table>foster<tr><td><b a=b>one<i>two</b>three</i></table><template><svg><foreignObject><p title='&amp;'>four</p></foreignObject></svg></template><!--tail-->";
    let root = Budget::new(1 << 20).unwrap();
    let child = root.child(1 << 20).unwrap();
    let baseline = root.used();
    let mut completed = false;
    for allowance in 0..512 {
        let result = with_reservation_limit(allowance, || {
            parse_with_budget(input, ParseLimits::default(), &child)
        });
        match result {
            Err(ParseError::Allocation) => {}
            Ok(document) => {
                invariants(&document);
                assert!(root.used() > baseline);
                drop(document);
                completed = true;
            }
            other => panic!("unexpected parse result at {allowance}: {other:?}"),
        }
        assert_eq!(root.used(), baseline, "allowance {allowance}");
        if completed {
            break;
        }
    }
    assert!(completed);
    let document = parse_with_budget(input, ParseLimits::default(), &child).unwrap();
    invariants(&document);
    drop(document);
    assert_eq!(root.used(), baseline);
}

#[test]
fn a_document_keeps_its_page_admission_after_the_original_handles_drop() {
    use td_alloc::Budget;
    let root = Budget::new(1 << 20).unwrap();
    let child = root.child(1 << 20).unwrap();
    let document =
        parse_with_budget("<p a=b>retained text", ParseLimits::default(), &child).unwrap();
    let retained = root.used();
    drop(child);
    assert_eq!(root.used(), retained);
    assert!(matches!(
        root.charge(root.limit() - retained + 1),
        Err(td_alloc::BudgetError::Limit)
    ));
    let spare = root.charge(root.limit() - retained).unwrap();
    assert!(matches!(
        parse_with_budget("<p>second", ParseLimits::default(), &root),
        Err(ParseError::Bytes)
    ));
    assert_eq!(root.used(), root.limit());
    drop(spare);
    invariants(&document);
    drop(document);
    assert_eq!(root.used(), Budget::metadata_bytes());
}

#[test]
fn batched_text_and_ascii_tokens_match_scalar_recovery_and_work() {
    let pieces = [
        "<body><p>ordinary words &amp; more</p>",
        "<p a='one two' data-FOO=x:y.z>é語\r\ntext</p>",
        "<pre>\nfirst\nsecond</pre>",
        "<b>one<i>two</b>three</i>",
        "<table>foster<tr><td>cell</td></tr>outside</table>",
        "<svg><foreignObject><p>integration</p></foreignObject></svg>",
        "<template><p>content</template>",
        "<svg><foreignObject><![CDATA[]]></foreignObject></svg>",
        "<svg><title><![CDATA[]]></title></svg>",
        "<math><mi><![CDATA[]]></mi></math>",
        "<textarea>raw &amp; text</textarea>",
        "<p a=b c=\"bad\x7fvalue\" a='duplicate'>\0tail</p>",
        "<head>stray</head><body><span>x</span>tail",
    ];
    let root = td_alloc::Budget::new(1 << 20).unwrap();
    for (i, first) in pieces.iter().enumerate() {
        for second in pieces {
            let input = format!("{first}{second}{first}");
            let fast = parse_with_budget(&input, ParseLimits::default(), &root).unwrap();
            let mut builder = builder::Builder::new(&input, ParseLimits::default(), &root).unwrap();
            builder.scalar_text = true;
            builder.tok.scalar_runs = true;
            let scalar = builder.parse().unwrap();
            assert_eq!(
                format!("{fast:?}"),
                format!("{scalar:?}"),
                "case {i}: {input:?}"
            );
            invariants(&fast);
        }
    }
    assert_eq!(root.used(), td_alloc::Budget::metadata_bytes());
}

#[test]
fn empty_cdata_at_integration_points_keeps_node_admission_unchanged() {
    for input in [
        "<svg><foreignObject><![CDATA[]]></foreignObject></svg>",
        "<svg><title><![CDATA[]]></title></svg>",
        "<math><mi><![CDATA[]]></mi></math>",
    ] {
        let limits = ParseLimits {
            nodes: 6,
            ..ParseLimits::default()
        };
        let document = parse_with_limits(input, limits).unwrap();
        assert_eq!(document.len(), 6);
        assert!(!document
            .nodes()
            .any(|(_, node)| matches!(node.kind(), NodeKind::Text(text) if text.is_empty())));
    }
}
