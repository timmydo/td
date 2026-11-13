#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
use crate::selectors::Element;

pub(crate) struct Node {
    pub tag: &'static str,
    pub attrs: Vec<(&'static str, String)>,
    pub parent: Option<usize>,
    pub previous: Option<usize>,
    pub next: Option<usize>,
    pub first: Option<usize>,
    pub text: bool,
    pub states: Vec<crate::selectors::State>,
}

impl Node {
    fn new(
        tag: &'static str,
        parent: Option<usize>,
        attrs: &[(&'static str, &str)],
        text: bool,
    ) -> Self {
        Self {
            tag,
            parent,
            attrs: attrs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect(),
            previous: None,
            next: None,
            first: None,
            text,
            states: Vec::new(),
        }
    }
}

pub(crate) fn tree() -> Vec<Node> {
    let mut nodes = vec![
        Node::new("html", None, &[], false),
        Node::new("body", Some(0), &[], false),
        Node::new(
            "div",
            Some(1),
            &[("id", "container"), ("class", "zone")],
            false,
        ),
        Node::new(
            "p",
            Some(2),
            &[
                ("class", "hot first"),
                ("lang", "en-US"),
                ("data-x", "Alpha Beta"),
            ],
            false,
        ),
        Node::new("span", Some(2), &[("class", "second")], true),
        Node::new("p", Some(2), &[("class", "hot last")], false),
        Node::new("a", Some(5), &[("id", "target"), ("href", "/")], false),
        Node::new("aside", Some(1), &[], false),
        Node::new("p", Some(7), &[("class", "hot")], false),
    ];
    for index in 0..nodes.len() {
        if let Some(parent) = nodes[index].parent {
            if nodes[parent].first.is_none() {
                nodes[parent].first = Some(index);
            }
            if let Some(previous) = (0..index)
                .rev()
                .find(|&other| nodes[other].parent == Some(parent))
            {
                nodes[index].previous = Some(previous);
                nodes[previous].next = Some(index);
            }
        }
    }
    nodes
}

#[derive(Clone, Copy)]
pub(crate) struct Handle<'a> {
    pub nodes: &'a [Node],
    pub index: usize,
}

impl Element for Handle<'_> {
    fn state(&self, state: crate::selectors::State) -> bool {
        self.nodes[self.index].states.contains(&state)
    }
    fn tag_name(&self) -> &str {
        self.nodes[self.index].tag
    }
    fn attribute(&self, name: &str) -> Option<&str> {
        self.nodes[self.index]
            .attrs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }
    fn parent(&self) -> Option<Self> {
        self.nodes[self.index]
            .parent
            .map(|index| Self { index, ..*self })
    }
    fn previous_sibling(&self) -> Option<Self> {
        self.nodes[self.index]
            .previous
            .map(|index| Self { index, ..*self })
    }
    fn next_sibling(&self) -> Option<Self> {
        self.nodes[self.index]
            .next
            .map(|index| Self { index, ..*self })
    }
    fn first_child(&self) -> Option<Self> {
        self.nodes[self.index]
            .first
            .map(|index| Self { index, ..*self })
    }
    fn has_text(&self) -> bool {
        self.nodes[self.index].text
    }
    fn is_link(&self) -> bool {
        self.tag_name() == "a" && self.attribute("href").is_some()
    }
}

pub(crate) fn owned_text(value: &str) -> crate::syntax::Text {
    crate::syntax::text(&crate::memory().unwrap(), value).unwrap()
}
pub(crate) fn families(
    values: Vec<crate::values::Family>,
) -> td_alloc::BudgetShared<td_alloc::BudgetVec<crate::values::Family>> {
    let memory = crate::memory().unwrap();
    let mut output = td_alloc::BudgetVec::new(&memory).unwrap();
    for value in values {
        output.try_push(value).unwrap();
    }
    td_alloc::BudgetShared::new(&memory, output).unwrap()
}
