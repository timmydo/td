use td_alloc::{BudgetShared, BudgetString, BudgetVec};
// Immutable arena-indexed HTML documents. Parsing never invokes scripts or I/O.
use crate::html5::{self, ParseError, ParseLimits, Token, Tokenizer};
use std::num::NonZeroU32;
mod builder;
mod modes;
mod tables;
use modes::ws;
mod foreign;
mod formatting;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(NonZeroU32);
impl NodeId {
    fn new(index: usize) -> Result<Self, ParseError> {
        let value = u32::try_from(index)
            .ok()
            .and_then(|v| v.checked_add(1))
            .and_then(NonZeroU32::new)
            .ok_or(ParseError::Nodes)?;
        Ok(Self(value))
    }
    pub fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    Html,
    Svg,
    MathMl,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeNamespace {
    None,
    Xml,
    Xmlns,
    XLink,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributePrefix {
    Xml,
    Xmlns,
    XLink,
}
impl AttributePrefix {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xml => "xml",
            Self::Xmlns => "xmlns",
            Self::XLink => "xlink",
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: BudgetShared<BudgetString>,
    pub prefix: Option<AttributePrefix>,
    pub namespace: AttributeNamespace,
    pub value: BudgetString,
}
#[derive(Debug)]
pub struct Element {
    pub name: BudgetShared<BudgetString>,
    pub namespace: Namespace,
    pub attributes: BudgetVec<Attribute>,
    pub template_contents: Option<NodeId>,
    html_integration: bool,
}
impl Element {
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|a| {
                #[cfg(test)]
                ATTRIBUTE_VISITS.with(|v| v.set(v.get() + 1));
                a.namespace == AttributeNamespace::None && a.name == name
            })
            .map(|a| a.value.as_str())
    }
}
#[derive(Debug)]
pub enum NodeKind {
    Document,
    DocumentFragment,
    Element(Element),
    Text(BudgetString),
    Comment(BudgetString),
    Doctype(BudgetShared<html5::Doctype>),
}
#[derive(Debug)]
pub struct Node {
    kind: NodeKind,
    parent: Option<NodeId>,
    first: Option<NodeId>,
    last: Option<NodeId>,
    previous: Option<NodeId>,
    next: Option<NodeId>,
}
impl Node {
    pub fn kind(&self) -> &NodeKind {
        &self.kind
    }
    pub fn element(&self) -> Option<&Element> {
        if let NodeKind::Element(e) = &self.kind {
            Some(e)
        } else {
            None
        }
    }
    pub fn text(&self) -> Option<&str> {
        if let NodeKind::Text(s) = &self.kind {
            Some(s)
        } else {
            None
        }
    }
    pub fn parent(&self) -> Option<NodeId> {
        self.parent
    }
    pub fn first_child(&self) -> Option<NodeId> {
        self.first
    }
    pub fn last_child(&self) -> Option<NodeId> {
        self.last
    }
    pub fn previous_sibling(&self) -> Option<NodeId> {
        self.previous
    }
    pub fn next_sibling(&self) -> Option<NodeId> {
        self.next
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuirksMode {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}
#[derive(Debug)]
pub struct Document {
    nodes: BudgetVec<Node>,
    root: NodeId,
    quirks: QuirksMode,
    errors: usize,
    work: usize,
    charged_bytes: usize,
}
impl Document {
    pub fn budget(&self) -> &td_alloc::Budget {
        self.nodes.budget()
    }
    pub fn root(&self) -> NodeId {
        self.root
    }
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
    pub fn quirks_mode(&self) -> QuirksMode {
        self.quirks
    }
    pub fn errors(&self) -> usize {
        self.errors
    }
    pub fn work(&self) -> usize {
        self.work
    }
    pub fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            document: self,
            next: self.node(id).and_then(Node::first_child),
        }
    }
    pub fn nodes(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(i, n)| NodeId::new(i).ok().map(|id| (id, n)))
    }
}
pub struct Children<'a> {
    document: &'a Document,
    next: Option<NodeId>,
}
impl<'a> Iterator for Children<'a> {
    type Item = (NodeId, &'a Node);
    fn next(&mut self) -> Option<Self::Item> {
        let id = self.next?;
        let node = self.document.node(id)?;
        self.next = node.next;
        Some((id, node))
    }
}
pub fn parse(input: &str) -> Result<Document, ParseError> {
    parse_with_limits(input, ParseLimits::default())
}
pub fn parse_with_limits(input: &str, limits: ParseLimits) -> Result<Document, ParseError> {
    if input.len() > limits.clamped().input {
        return Err(ParseError::Input);
    }
    let memory =
        td_alloc::Budget::new(crate::html5::MAX_TREE_BYTES + td_alloc::Budget::metadata_bytes())?;
    parse_with_budget(input, limits, &memory)
}
/// Parse with a shared budget retained by every owned tree allocation.
pub fn parse_with_budget(
    input: &str,
    limits: ParseLimits,
    memory: &td_alloc::Budget,
) -> Result<Document, ParseError> {
    builder::Builder::new(input, limits, memory)?.parse()
}
#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! { static ATTRIBUTE_VISITS: std::cell::Cell<usize> = const {std::cell::Cell::new(0)}; }
