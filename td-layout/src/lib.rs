//! Bounded, source-preserving normal flow. See DESIGN.md for the subset.
#![forbid(unsafe_code)]
pub mod block;
pub mod inline;
use std::mem::size_of;
use td_css::selectors::Element;
pub use td_css::sharing::StyleId;
use td_css::sharing::{PoolLimits, StylePool};
use td_css::{ComputedStyle, ComputedValue, Property, StyleSet};
use td_dom::{HtmlTree, StateProvider};
use td_html::document::{Document, NodeId};

pub const MAX_PAGE_INPUT: usize = 2 * 1024 * 1024;
pub const MAX_PAGE_BYTES: usize = 224 * 1024 * 1024;
pub const WORKING_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BOXES: usize = 262_144;
pub const MAX_DEPTH: usize = 256;
pub const MAX_LAYOUT_WORK: usize = 64_000_000;
pub const MAX_LINE_SCALARS: usize = 65_536;
pub const MAX_LINES: usize = 1_048_576;
pub const MAX_FRAGMENTS: usize = 1_048_576;
pub const MAX_COORDINATE: f64 = 1_000_000_000.0;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Limit(&'static str),
    Css(td_css::Error),
    Tree(td_dom::Error),
    Invalid,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "layout refused: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<td_css::Error> for Error {
    fn from(value: td_css::Error) -> Self {
        Self::Css(value)
    }
}
impl From<td_dom::Error> for Error {
    fn from(value: td_dom::Error) -> Self {
        Self::Tree(value)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub input: usize,
    pub bytes: usize,
    pub boxes: usize,
    pub depth: usize,
    pub work: usize,
    pub line_scalars: usize,
    pub lines: usize,
    pub fragments: usize,
    pub styles: PoolLimits,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            input: MAX_PAGE_INPUT,
            bytes: MAX_PAGE_BYTES,
            boxes: MAX_BOXES,
            depth: MAX_DEPTH,
            work: MAX_LAYOUT_WORK,
            line_scalars: MAX_LINE_SCALARS,
            lines: MAX_LINES,
            fragments: MAX_FRAGMENTS,
            styles: PoolLimits::default(),
        }
    }
}
impl Limits {
    fn clamped(self) -> Self {
        Self {
            input: self.input.min(MAX_PAGE_INPUT),
            bytes: self.bytes.min(MAX_PAGE_BYTES),
            boxes: self.boxes.min(MAX_BOXES),
            depth: self.depth.min(MAX_DEPTH),
            work: self.work.min(MAX_LAYOUT_WORK),
            line_scalars: self.line_scalars.min(MAX_LINE_SCALARS),
            lines: self.lines.min(MAX_LINES),
            fragments: self.fragments.min(MAX_FRAGMENTS),
            styles: self.styles,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxId(usize);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    Block,
    Inline,
    Text,
    Image,
    Break,
}
#[derive(Clone, Copy, Debug)]
pub struct BoxNode {
    pub node: NodeId,
    pub style: StyleId,
    pub kind: BoxKind,
    pub parent: Option<BoxId>,
    pub first_child: Option<BoxId>,
    pub next_sibling: Option<BoxId>,
}
/// Borrowed immutable text; one style per equivalence class, never per node.
pub struct StyledDocument<'d> {
    document: &'d Document,
    styles: StylePool,
    boxes: Vec<BoxNode>,
    first: Option<BoxId>,
    charged: usize,
    limits: Limits,
}
impl<'d> StyledDocument<'d> {
    pub fn new(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits,
    ) -> Result<Self, Error> {
        Self::build(document, sheets, input_bytes, limits, None)
    }
    pub fn with_state(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits,
        state: &'d dyn StateProvider,
    ) -> Result<Self, Error> {
        Self::build(document, sheets, input_bytes, limits, Some(state))
    }
    fn build(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits,
        state: Option<&'d dyn StateProvider>,
    ) -> Result<Self, Error> {
        let limits = limits.clamped();
        if input_bytes > limits.input {
            return Err(Error::Limit("MAX_PAGE_INPUT"));
        }
        // Reserve all fixed arrays and navigation before constructing them.
        // Navigation and the temporary node map remain charged after dropping.
        let fixed = document
            .len()
            .checked_mul(
                td_dom::MAX_INDEX_BYTES / td_dom::MAX_INDEX_NODES
                    + size_of::<Option<StyleId>>()
                    + size_of::<BoxNode>()
                    + 4 * size_of::<Option<BoxId>>(),
            )
            .and_then(|n| n.checked_add(document.charged_bytes()))
            .and_then(|n| n.checked_add(input_bytes + 1024))
            .filter(|n| *n <= limits.bytes)
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        let tree = match state {
            Some(state) => HtmlTree::with_state(document, state)?,
            None => HtmlTree::new(document)?,
        };
        let mut styles = StylePool::new(PoolLimits {
            bytes: limits.styles.bytes.min(limits.bytes - fixed),
            ..limits.styles
        });
        let mut node_styles = vec![None; document.len()];
        let mut root_size = 16.0;
        for element in tree.elements() {
            let parent_id = element.parent().map(|p| p.node_id());
            let parent_style =
                parent_id.and_then(|p| node_styles.get(p.index()).copied().flatten());
            if parent_id.is_some() && parent_style.is_none() {
                continue;
            }
            let computed = sheets.compute(
                &element,
                parent_style.and_then(|id| styles.get(id)),
                root_size,
                element.attribute("style").unwrap_or(""),
            )?;
            if parent_id.is_none() {
                root_size = computed.font_size();
            }
            if keyword(&computed, Property::Display) == "none" {
                continue;
            }
            let id = styles.intern(computed)?;
            *node_styles
                .get_mut(element.node_id().index())
                .ok_or(Error::Invalid)? = Some(id);
        }
        let mut out = Self {
            document,
            charged: fixed + styles.charged_bytes(),
            styles,
            boxes: Vec::with_capacity(document.len()),
            first: None,
            limits,
        };
        let mut last = vec![None; document.len()];
        let mut parents = vec![None; document.len()];
        let mut depths = vec![0usize; document.len()];
        let mut previous_root: Option<BoxId> = None;
        // Walk actual links; repaired HTML need not follow allocation order.
        let mut next = document.node(document.root()).and_then(|n| n.first_child());
        while let Some(node_id) = next {
            let node = document.node(node_id).ok_or(Error::Invalid)?;
            let parent = node
                .parent()
                .and_then(|p| parents.get(p.index()).copied().flatten());
            let style = node_styles
                .get(node_id.index())
                .copied()
                .flatten()
                .or_else(|| {
                    node.text()
                        .and(parent)
                        .and_then(|p: BoxId| out.boxes.get(p.0).map(|b| b.style))
                });
            let mut descend = false;
            if let Some(style) = style {
                let depth = node
                    .parent()
                    .and_then(|p| depths.get(p.index()).copied())
                    .unwrap_or(0)
                    + 1;
                if depth > limits.depth {
                    return Err(Error::Limit("MAX_DEPTH"));
                }
                if out.boxes.len() >= limits.boxes {
                    return Err(Error::Limit("MAX_BOXES"));
                }
                let computed = out.styles.get(style).ok_or(Error::Invalid)?;
                let kind = if node.text().is_some() {
                    BoxKind::Text
                } else {
                    match node.element().map(|e| e.name.as_str()) {
                        Some("img") => BoxKind::Image,
                        Some("br") => BoxKind::Break,
                        _ if keyword(computed, Property::Display) == "inline" => BoxKind::Inline,
                        _ => BoxKind::Block,
                    }
                };
                let id = BoxId(out.boxes.len());
                out.boxes.push(BoxNode {
                    node: node_id,
                    style,
                    kind,
                    parent,
                    first_child: None,
                    next_sibling: None,
                });
                *parents.get_mut(node_id.index()).ok_or(Error::Invalid)? = Some(id);
                *depths.get_mut(node_id.index()).ok_or(Error::Invalid)? = depth;
                let previous = match parent {
                    Some(p) => last.get(p.0).copied().flatten(),
                    None => previous_root,
                };
                if let Some(p) = previous {
                    out.boxes.get_mut(p.0).ok_or(Error::Invalid)?.next_sibling = Some(id);
                } else if let Some(p) = parent {
                    out.boxes.get_mut(p.0).ok_or(Error::Invalid)?.first_child = Some(id);
                } else {
                    out.first = Some(id);
                }
                if let Some(p) = parent {
                    *last.get_mut(p.0).ok_or(Error::Invalid)? = Some(id);
                } else {
                    previous_root = Some(id);
                }
                descend = !matches!(kind, BoxKind::Image | BoxKind::Break);
            }
            if descend {
                if let Some(child) = node.first_child() {
                    next = Some(child);
                    continue;
                }
            }
            let mut cursor = node_id;
            loop {
                let node = document.node(cursor).ok_or(Error::Invalid)?;
                if let Some(sibling) = node.next_sibling() {
                    next = Some(sibling);
                    break;
                }
                match node.parent() {
                    Some(p) if p != document.root() => cursor = p,
                    _ => {
                        next = None;
                        break;
                    }
                }
            }
        }
        Ok(out)
    }
    pub fn document(&self) -> &'d Document {
        self.document
    }
    pub fn boxes(&self) -> &[BoxNode] {
        &self.boxes
    }
    pub fn box_node(&self, id: BoxId) -> Option<&BoxNode> {
        self.boxes.get(id.0)
    }
    pub fn first_box(&self) -> Option<BoxId> {
        self.first
    }
    pub fn style(&self, id: StyleId) -> Option<&ComputedStyle> {
        self.styles.get(id)
    }
    pub fn style_work(&self) -> usize {
        self.styles.work()
    }
    pub fn style_count(&self) -> usize {
        self.styles.len()
    }
    pub fn charged_bytes(&self) -> usize {
        self.charged
    }
    pub fn limits(&self) -> Limits {
        self.limits
    }
}
fn keyword(style: &ComputedStyle, property: Property) -> &str {
    match style.get(property) {
        Some(ComputedValue::Keyword(s)) => s,
        _ => "",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use td_css::{Origin, HTML_UA_CSS};
    fn sheets(css: &str) -> StyleSet {
        StyleSet::new(&[(Origin::UserAgent, HTML_UA_CSS), (Origin::Author, css)]).unwrap()
    }
    #[test]
    fn equal_siblings_share_and_hidden_foreign_template_children_do_not_generate_boxes() {
        let html = "<p>one<p>two<div hidden>gone</div><svg><foreignObject><p>foreign</p></foreignObject></svg><template>hidden</template>";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &dom,
            &sheets("[hidden]{display:none}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let ps: Vec<_> = styled
            .boxes()
            .iter()
            .filter(|b| {
                dom.node(b.node)
                    .and_then(|n| n.element())
                    .is_some_and(|e| e.name == "p")
            })
            .collect();
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].style, ps[1].style);
        let texts: Vec<_> = styled
            .boxes()
            .iter()
            .filter_map(|b| dom.node(b.node).and_then(|n| n.text()))
            .collect();
        assert_eq!(texts, ["one", "two"]);
    }
    #[test]
    fn custom_maps_are_part_of_identity_and_parent_inheritance() {
        let html = "<div style='--x:10px'><p>a<p>b</div><div style='--x:20px'><p>c</div>";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(
            &dom,
            &sheets("p{margin-left:var(--x)}"),
            html.len(),
            Limits::default(),
        )
        .unwrap();
        let ps: Vec<_> = styled
            .boxes()
            .iter()
            .filter(|b| {
                dom.node(b.node)
                    .and_then(|n| n.element())
                    .is_some_and(|e| e.name == "p")
            })
            .collect();
        assert_eq!(ps[0].style, ps[1].style);
        assert_ne!(ps[0].style, ps[2].style);
        assert_eq!(
            styled.style(ps[2].style).unwrap().get(Property::MarginLeft),
            Some(&ComputedValue::Length(td_css::values::Length::Px(20.0)))
        );
    }
    #[test]
    fn page_and_box_limits_at_edges() {
        let dom = td_html::document::parse("<p>x").unwrap();
        let sheet = sheets("");
        let styled = StyledDocument::new(&dom, &sheet, 4, Limits::default()).unwrap();
        let count = styled.boxes().len();
        let bytes = styled.charged_bytes();
        for (limit, ok) in [(count, true), (count - 1, false)] {
            assert_eq!(
                StyledDocument::new(
                    &dom,
                    &sheet,
                    4,
                    Limits {
                        boxes: limit,
                        ..Limits::default()
                    }
                )
                .is_ok(),
                ok
            );
        }
        for (limit, ok) in [(bytes, true), (bytes - 1, false)] {
            assert_eq!(
                StyledDocument::new(
                    &dom,
                    &sheet,
                    4,
                    Limits {
                        bytes: limit,
                        ..Limits::default()
                    }
                )
                .is_ok(),
                ok
            );
        }
        assert!(StyledDocument::new(&dom, &sheet, MAX_PAGE_INPUT, Limits::default()).is_ok());
        assert!(matches!(
            StyledDocument::new(&dom, &sheet, MAX_PAGE_INPUT + 1, Limits::default()),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
        assert!(StyledDocument::new(
            &dom,
            &sheet,
            4,
            Limits {
                depth: 4,
                ..Limits::default()
            }
        )
        .is_ok());
        assert!(matches!(
            StyledDocument::new(
                &dom,
                &sheet,
                4,
                Limits {
                    depth: 3,
                    ..Limits::default()
                }
            ),
            Err(Error::Limit("MAX_DEPTH"))
        ));
    }
}
