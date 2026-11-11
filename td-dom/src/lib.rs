//! HTML-only selector navigation over an immutable td-html document.
#![forbid(unsafe_code)]
use td_css::selectors::{Element, State};
use td_html::document::{Document, Namespace, NodeId, NodeKind};
pub const MAX_INDEX_NODES: usize = td_html::html5::MAX_NODES;
pub const MAX_INDEX_WORK: usize = MAX_INDEX_NODES * 2;
/// Maximum index payload, excluding allocator metadata and the borrowed tree.
pub const MAX_INDEX_BYTES: usize =
    MAX_INDEX_NODES * (std::mem::size_of::<Links>() + std::mem::size_of::<NodeId>());
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Nodes,
    Work,
    InvalidTree,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTML selector index refused: {self:?}")
    }
}
impl std::error::Error for Error {}
/// Stable for a matching operation. The caller owns focus, hover propagation
/// and checked/selected semantics. Browsing history has no query.
pub trait StateProvider {
    fn state(&self, node: NodeId, state: State) -> bool;
}
struct NoState;
impl StateProvider for NoState {
    fn state(&self, _node: NodeId, _state: State) -> bool {
        false
    }
}
static NO_STATE: NoState = NoState;
#[derive(Clone, Copy, Default)]
struct Links {
    parent: Option<NodeId>,
    previous: Option<NodeId>,
    next: Option<NodeId>,
    first: Option<NodeId>,
    last: Option<NodeId>,
    eligible: bool,
    text: bool,
}
struct Work {
    left: usize,
}
impl Work {
    fn step(&mut self) -> Result<(), Error> {
        self.left = self.left.checked_sub(1).ok_or(Error::Work)?;
        Ok(())
    }
}
fn check_size(nodes: usize) -> Result<(), Error> {
    if nodes > MAX_INDEX_NODES {
        Err(Error::Nodes)
    } else {
        Ok(())
    }
}
/// An HTML projection. Foreign subtrees and template fragments are excluded;
/// structural selectors, including :empty, observe this projection.
/// Navigation is indexed once so comments/text never make CSS callbacks scan.
pub struct HtmlTree<'d> {
    document: &'d Document,
    links: Vec<Links>,
    order: Vec<NodeId>,
    state: &'d dyn StateProvider,
}
impl<'d> HtmlTree<'d> {
    pub fn new(document: &'d Document) -> Result<Self, Error> {
        Self::with_state(document, &NO_STATE)
    }
    pub fn with_state(document: &'d Document, state: &'d dyn StateProvider) -> Result<Self, Error> {
        check_size(document.len())?;
        let mut tree = Self {
            document,
            links: vec![Links::default(); document.len()],
            order: Vec::with_capacity(document.len()),
            state,
        };
        let mut work = Work {
            left: MAX_INDEX_WORK,
        };
        let mut next = document
            .node(document.root())
            .ok_or(Error::InvalidTree)?
            .first_child();
        while let Some(id) = next {
            work.step()?;
            let n = document.node(id).ok_or(Error::InvalidTree)?;
            let mut descend = true;
            match n.kind() {
                NodeKind::Element(e) if e.namespace == Namespace::Html => {
                    let parent = n
                        .parent()
                        .filter(|p| tree.links.get(p.index()).is_some_and(|l| l.eligible));
                    let previous =
                        parent.and_then(|p| tree.links.get(p.index()).and_then(|l| l.last));
                    let link = tree.links.get_mut(id.index()).ok_or(Error::InvalidTree)?;
                    link.eligible = true;
                    link.parent = parent;
                    link.previous = previous;
                    if let Some(previous) = previous {
                        tree.links
                            .get_mut(previous.index())
                            .ok_or(Error::InvalidTree)?
                            .next = Some(id);
                    }
                    if let Some(parent) = parent {
                        let link = tree
                            .links
                            .get_mut(parent.index())
                            .ok_or(Error::InvalidTree)?;
                        if link.first.is_none() {
                            link.first = Some(id);
                        }
                        link.last = Some(id);
                    }
                    tree.order.push(id);
                }
                NodeKind::Element(_) => descend = false,
                NodeKind::Text(s) if !s.is_empty() => {
                    if let Some(parent) = n.parent() {
                        let l = tree
                            .links
                            .get_mut(parent.index())
                            .ok_or(Error::InvalidTree)?;
                        if l.eligible {
                            l.text = true;
                        }
                    }
                }
                _ => {}
            }
            if descend {
                if let Some(child) = n.first_child() {
                    next = Some(child);
                    continue;
                }
            }
            let mut current = id;
            loop {
                let node = document.node(current).ok_or(Error::InvalidTree)?;
                if let Some(sibling) = node.next_sibling() {
                    next = Some(sibling);
                    break;
                }
                match node.parent() {
                    Some(parent) if parent != document.root() => {
                        work.step()?;
                        current = parent;
                    }
                    _ => {
                        next = None;
                        break;
                    }
                }
            }
        }
        Ok(tree)
    }
    pub fn document(&self) -> &'d Document {
        self.document
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    pub fn index_bytes(&self) -> usize {
        self.links.capacity() * std::mem::size_of::<Links>()
            + self.order.capacity() * std::mem::size_of::<NodeId>()
    }
    pub fn element(&self, id: NodeId) -> Option<ElementRef<'_, 'd>> {
        if !self.links.get(id.index())?.eligible {
            return None;
        }
        Some(ElementRef {
            tree: self,
            id,
            element: self.document.node(id)?.element()?,
        })
    }
    /// Actual document order, not arena allocation order after HTML repair.
    pub fn elements(&self) -> impl Iterator<Item = ElementRef<'_, 'd>> {
        self.order.iter().filter_map(|id| self.element(*id))
    }
}
#[derive(Clone, Copy)]
pub struct ElementRef<'t, 'd> {
    tree: &'t HtmlTree<'d>,
    id: NodeId,
    element: &'d td_html::document::Element,
}
impl ElementRef<'_, '_> {
    pub fn node_id(&self) -> NodeId {
        self.id
    }
}
impl<'t, 'd> Element for ElementRef<'t, 'd> {
    fn tag_name(&self) -> &str {
        &self.element.name
    }
    fn attribute(&self, name: &str) -> Option<&str> {
        self.element.attribute(name)
    }
    fn parent(&self) -> Option<Self> {
        self.tree
            .element(self.tree.links.get(self.id.index())?.parent?)
    }
    fn previous_sibling(&self) -> Option<Self> {
        self.tree
            .element(self.tree.links.get(self.id.index())?.previous?)
    }
    fn next_sibling(&self) -> Option<Self> {
        self.tree
            .element(self.tree.links.get(self.id.index())?.next?)
    }
    fn first_child(&self) -> Option<Self> {
        self.tree
            .element(self.tree.links.get(self.id.index())?.first?)
    }
    fn has_text(&self) -> bool {
        self.tree.links.get(self.id.index()).is_some_and(|l| l.text)
    }
    fn is_link(&self) -> bool {
        matches!(self.element.name.as_str(), "a" | "area")
            && self.element.attribute("href").is_some()
    }
    fn state(&self, state: State) -> bool {
        self.tree.state.state(self.id, state)
    }
}
#[cfg(test)]
mod tests;
