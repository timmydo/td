//! Bounded, source-preserving normal flow. See DESIGN.md for the subset.
#![forbid(unsafe_code)]
pub mod block;
pub mod inline;
pub mod list;
pub mod text;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use td_alloc::{Budget, BudgetShared, BudgetVec};
use td_css::selectors::Element;
pub use td_css::sharing::StyleId;
use td_css::sharing::{PoolLimits, StylePool};
use td_css::{ComputedStyle, ComputedValue, Property, StyleSet};
use td_dom::{HtmlTree, StateProvider};
use td_html::document::{Document, NodeId};

/// Default input admission: a page's markup.
pub const MAX_PAGE_INPUT: usize = 2 * 1024 * 1024;
/// The largest Limits::input a caller may set: a text page's one text node.
pub const MAX_INPUT: usize = td_html::html5::MAX_INPUT_BYTES;
pub const MAX_PAGE_BYTES: usize = 224 * 1024 * 1024;
pub const WORKING_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BOXES: usize = 262_144;
pub const MAX_DEPTH: usize = 256;
pub const MAX_LAYOUT_WORK: usize = 64_000_000;
pub const MAX_LINE_SCALARS: usize = 65_536;
/// The most scalars one text fragment holds: contiguous source past it
/// starts another fragment, so a painter skips by fragments' x what lies
/// before its clip and replays at most this many scalars to find it
/// (DESIGN.md §3).
pub const MAX_FRAGMENT_SCALARS: usize = 1_024;
pub const MAX_LINES: usize = 1_048_576;
pub const MAX_FRAGMENTS: usize = 1_048_576;
pub const MAX_COORDINATE: f64 = 1_000_000_000.0;
/// Columns one table's grid may hold (DESIGN.md §5, "Tables").
pub const MAX_TABLE_COLUMNS: usize = 4096;
/// HTML's ceilings on a cell's `colspan` and `rowspan` (HTML §4.9.11).
pub const MAX_COLSPAN: u32 = 1000;
pub const MAX_ROWSPAN: u32 = 65534;
/// Weighted cascade work per page, summed as CascadeWork::total.
pub const MAX_CASCADE_WORK: usize = 128_000_000;
// Unit weights (DESIGN.md §6): no measured shape costs much over 25 ns a
// unit on the measured host.
const ELEMENT_WORK: usize = 64;
const COMPONENT_WORK: usize = 5;
// A candidate rule costs more as an element gathers more of them.
const CANDIDATES_PER_EXTRA_WORK: usize = 2048;
const CUSTOM_DECLARATION_WORK: usize = 6;
const COPY_WORK: usize = 6;
// A locally declared custom name costs more as its element's map grows.
const CUSTOM_WORK_PER_BIT: usize = 6;
const CUSTOM_WORK_OFFSET: usize = 12;
const CUSTOM_WORK_MIN: usize = 8;
const MATCH_BYTES_PER_WORK: usize = 16;
const SUBSTITUTION_BYTES_PER_WORK: usize = 64;
/// Layout work units between cancel-flag reads.
pub(crate) const CANCEL_INTERVAL: usize = 4096;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Limit(&'static str),
    Css(td_css::Error),
    Tree(td_dom::Error),
    Invalid,
    Memory(td_alloc::BudgetError),
    /// The caller's cancel flag was set; nothing built is kept.
    Cancelled,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "layout refused: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<td_alloc::BudgetError> for Error {
    fn from(value: td_alloc::BudgetError) -> Self {
        Self::Memory(value)
    }
}
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
pub struct Limits<'c> {
    /// Input bytes admitted; MAX_PAGE_INPUT by default, at most MAX_INPUT.
    pub input: usize,
    /// The ceiling on construction's `Charges::construction` and, unless a
    /// request sets its own, a layout's `page_bytes`.
    pub bytes: usize,
    pub boxes: usize,
    pub depth: usize,
    pub work: usize,
    pub line_scalars: usize,
    pub lines: usize,
    pub fragments: usize,
    pub styles: PoolLimits,
    /// Ceiling on the weighted sum of CascadeWork over the whole cascade.
    pub cascade_work: usize,
    /// False ignores every element's `style` attribute without reading it.
    pub style_attributes: bool,
    /// Read between elements while cascading; another thread may set it.
    pub cancel: Option<&'c AtomicBool>,
}
impl Default for Limits<'_> {
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
            cascade_work: MAX_CASCADE_WORK,
            style_attributes: true,
            cancel: None,
        }
    }
}
impl<'c> Limits<'c> {
    fn clamped(self) -> Self {
        Self {
            input: self.input.min(MAX_INPUT),
            bytes: self.bytes.min(MAX_PAGE_BYTES),
            boxes: self.boxes.min(MAX_BOXES),
            depth: self.depth.min(MAX_DEPTH),
            work: self.work.min(MAX_LAYOUT_WORK),
            // A line holds at least one item, so every line makes progress.
            line_scalars: self.line_scalars.clamp(1, MAX_LINE_SCALARS),
            lines: self.lines.min(MAX_LINES),
            fragments: self.fragments.min(MAX_FRAGMENTS),
            styles: self.styles,
            cascade_work: self.cascade_work.min(MAX_CASCADE_WORK),
            style_attributes: self.style_attributes,
            cancel: self.cancel,
        }
    }
}
pub(crate) fn cancelled(flag: Option<&AtomicBool>) -> bool {
    flag.is_some_and(|f| f.load(Ordering::Relaxed))
}
/// What a cascade spent, summed over its elements from td-css's
/// ComputeStats. `total` is the weighted sum Limits::cascade_work bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CascadeWork {
    pub elements: usize,
    pub candidate_rules: usize,
    pub match_steps: usize,
    pub match_bytes: usize,
    pub declarations: usize,
    pub custom_declarations: usize,
    pub local_custom_properties: usize,
    pub custom_copies: usize,
    pub substitution_components: usize,
    pub substitution_bytes: usize,
    pub total: usize,
}
impl CascadeWork {
    /// One element's weighted units.
    fn units(stats: &td_css::ComputeStats) -> Option<usize> {
        let local = stats.local_custom_properties;
        let bits = local.checked_ilog2().map_or(0, |b| b as usize + 1);
        let per_name = (CUSTOM_WORK_PER_BIT * bits)
            .saturating_sub(CUSTOM_WORK_OFFSET)
            .max(CUSTOM_WORK_MIN);
        let candidates = stats.candidate_rules;
        let terms = [
            (stats.match_steps.saturating_sub(stats.match_bytes), 1),
            (candidates, 1 + candidates / CANDIDATES_PER_EXTRA_WORK),
            (stats.declarations, 1),
            (stats.custom_declarations, CUSTOM_DECLARATION_WORK),
            (stats.custom_copies, COPY_WORK),
            (stats.substitution_components, COMPONENT_WORK),
            (local, per_name),
            (stats.match_bytes / MATCH_BYTES_PER_WORK, 1),
            (stats.substitution_bytes / SUBSTITUTION_BYTES_PER_WORK, 1),
        ];
        let mut units = ELEMENT_WORK;
        for (count, weight) in terms {
            units = units.checked_add(count.checked_mul(weight)?)?;
        }
        Some(units)
    }
    fn add(&mut self, stats: &td_css::ComputeStats, ceiling: usize) -> Result<(), Error> {
        let units = Self::units(stats);
        self.elements = self.elements.saturating_add(1);
        self.candidate_rules = self.candidate_rules.saturating_add(stats.candidate_rules);
        self.match_steps = self.match_steps.saturating_add(stats.match_steps);
        self.match_bytes = self.match_bytes.saturating_add(stats.match_bytes);
        self.declarations = self.declarations.saturating_add(stats.declarations);
        self.custom_declarations = self
            .custom_declarations
            .saturating_add(stats.custom_declarations);
        self.custom_copies = self.custom_copies.saturating_add(stats.custom_copies);
        self.local_custom_properties = self
            .local_custom_properties
            .saturating_add(stats.local_custom_properties);
        self.substitution_components = self
            .substitution_components
            .saturating_add(stats.substitution_components);
        self.substitution_bytes = self
            .substitution_bytes
            .saturating_add(stats.substitution_bytes);
        self.total = units
            .and_then(|n| n.checked_add(self.total))
            .filter(|n| *n <= ceiling)
            .ok_or(Error::Limit("MAX_CASCADE_WORK"))?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxId(usize);
impl BoxId {
    /// Its position in `StyledDocument::boxes`.
    pub fn index(self) -> usize {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxKind {
    Block,
    Inline,
    Text,
    Image,
    Break,
}
/// A box's part in CSS 2.2's table model (§17.2), by its display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableRole {
    /// `table`, laid as a block-level table, or `inline-table`, laid
    /// whole in its line (DESIGN.md §5, "Inline-blocks").
    Table,
    Caption,
    ColumnGroup,
    Column,
    /// `table-row-group`, `table-header-group` or `table-footer-group`.
    RowGroup,
    Row,
    Cell,
}
impl TableRole {
    pub fn of(display: &str) -> Option<Self> {
        Some(match display {
            "table" | "inline-table" => Self::Table,
            "table-caption" => Self::Caption,
            "table-column-group" => Self::ColumnGroup,
            "table-column" => Self::Column,
            "table-row-group" | "table-header-group" | "table-footer-group" => Self::RowGroup,
            "table-row" => Self::Row,
            "table-cell" => Self::Cell,
            _ => return None,
        })
    }
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
/// A styled document's declared charges, by the owner that holds each
/// (DESIGN.md §2, "Whole-page memory"). They are the conservative
/// admission model's terms, not td-alloc's measure of storage.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Charges {
    /// The document's declared charge, td-html's `charged_bytes`: a
    /// cumulative parse bound, discarded parse temporaries included, not
    /// the storage it holds. The document's owner holds it, once however
    /// many styled documents share it.
    pub tree: usize,
    /// The styled document's: its boxes, list items and style pool, which
    /// it holds, and the input, which the caller holds but it counts as
    /// its own from the cascade on. The pool's charge conservatively
    /// includes strings a computed style shares with its sheets.
    pub styled: usize,
    /// td-dom's navigation index, admitted while construction ran and
    /// returned when it ended.
    pub index: usize,
    /// Construction's own per-node arrays, admitted and returned as the
    /// index is.
    pub scratch: usize,
}
impl Charges {
    /// What the styled document and its tree hold once built, where a
    /// layout's admission starts. Two styled documents sharing one tree
    /// each count it: a ledger takes `tree` once.
    pub fn held(&self) -> usize {
        self.tree.saturating_add(self.styled)
    }
    /// What construction admitted at its peak, all four together, which
    /// `Limits::bytes` bounds.
    pub fn construction(&self) -> usize {
        self.held()
            .saturating_add(self.index)
            .saturating_add(self.scratch)
    }
}
/// Immutable text, borrowed or shared; one style per equivalence class.
pub struct StyledDocument<'d> {
    document: DocumentOwner<'d>,
    styles: StylePool,
    boxes: BudgetVec<BoxNode>,
    first: Option<BoxId>,
    geometry_count: usize,
    /// Charges but the tree's, which is its document's.
    styled_bytes: usize,
    index_bytes: usize,
    scratch_bytes: usize,
    limits: Limits<'static>,
    cascade: CascadeWork,
    /// Each list-item box's ordinal value, in box order.
    list_items: BudgetVec<list::ListItem>,
    /// List items with a marker: list-style-type is not none.
    marker_count: usize,
    /// Whether any block box has a table display; layout looks for tables
    /// only then.
    tables: bool,
    /// Whether any box floats (`float_side`); layout looks for floats
    /// and formatting contexts only then.
    floats: bool,
    /// Whether any box is an inline-block or an inline table
    /// (`inline_block`); layout lays boxes apart in a line only then.
    inline_blocks: bool,
    /// Whether any block box is a flex container (`flex_container`);
    /// layout lays flex lines only then.
    flex: bool,
}
enum DocumentOwner<'d> {
    Borrowed(&'d Document),
    Shared(BudgetShared<Document>),
}
impl<'d> StyledDocument<'d> {
    pub fn new(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
    ) -> Result<Self, Error> {
        Self::with_budget(
            document,
            sheets,
            input_bytes,
            limits,
            &default_budget(limits)?,
        )
    }
    pub fn with_state(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        state: &'d dyn StateProvider,
    ) -> Result<Self, Error> {
        Self::with_state_and_budget(
            document,
            sheets,
            input_bytes,
            limits,
            state,
            &default_budget(limits)?,
        )
    }
    /// Use the same identity for DOM navigation, candidate styles and boxes.
    pub fn with_budget(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        memory: &Budget,
    ) -> Result<Self, Error> {
        Self::build(document, sheets, input_bytes, limits, None, memory)
    }
    pub fn with_state_and_budget(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        state: &dyn StateProvider,
        memory: &Budget,
    ) -> Result<Self, Error> {
        Self::build(document, sheets, input_bytes, limits, Some(state), memory)
    }
    fn build(
        document: &'d Document,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        state: Option<&dyn StateProvider>,
        memory: &Budget,
    ) -> Result<Self, Error> {
        let limits = limits.clamped();
        let cancel = limits.cancel;
        if input_bytes > limits.input {
            return Err(Error::Limit("MAX_PAGE_INPUT"));
        }
        // Reserve all fixed arrays and navigation before constructing them,
        // beside the document's charge. Navigation and the per-node maps
        // are construction's alone, returned when it ends.
        let index_bytes = document
            .len()
            .checked_mul(td_dom::MAX_INDEX_BYTES / td_dom::MAX_INDEX_NODES)
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        let scratch_bytes = document
            .len()
            .checked_mul(size_of::<Option<StyleId>>() + 4 * size_of::<Option<BoxId>>())
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        let own = document
            .len()
            .checked_mul(size_of::<BoxNode>())
            .and_then(|n| n.checked_add(input_bytes + 1024))
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        let fixed = own
            .checked_add(index_bytes)
            .and_then(|n| n.checked_add(scratch_bytes))
            .and_then(|n| n.checked_add(document.charged_bytes()))
            .filter(|n| *n <= limits.bytes)
            .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
        // Read after the up-front limits, so they refuse by name first.
        if cancelled(cancel) {
            return Err(Error::Cancelled);
        }
        let tree = match state {
            Some(state) => HtmlTree::with_state_and_budget(document, state, memory)?,
            None => HtmlTree::with_budget(document, memory)?,
        };
        let mut styles = StylePool::with_budget(
            PoolLimits {
                bytes: limits.styles.bytes.min(limits.bytes - fixed),
                ..limits.styles
            },
            memory,
        )?;
        let mut node_styles = BudgetVec::filled(memory, document.len(), None)?;
        let mut root_size = 16.0;
        let mut cascade = CascadeWork::default();
        let mut stats = td_css::ComputeStats::default();
        for element in tree.elements() {
            if cancelled(cancel) {
                return Err(Error::Cancelled);
            }
            let parent_id = element.parent().map(|p| p.node_id());
            let parent_style =
                parent_id.and_then(|p| node_styles.get(p.index()).copied().flatten());
            if parent_id.is_some() && parent_style.is_none() {
                continue;
            }
            let inline = if limits.style_attributes {
                element.attribute("style").unwrap_or("")
            } else {
                ""
            };
            let computed = sheets.compute_with_stats_and_budget(
                &element,
                parent_style.and_then(|id| styles.get(id)),
                root_size,
                inline,
                &mut stats,
                memory,
            )?;
            cascade.add(&stats, limits.cascade_work)?;
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
            document: DocumentOwner::Borrowed(document),
            styled_bytes: own + styles.charged_bytes(),
            index_bytes,
            scratch_bytes,
            styles,
            boxes: td_alloc::BudgetVec::with_capacity(memory, document.len())?,
            first: None,
            geometry_count: 0,
            limits: Limits {
                cancel: None,
                ..limits
            },
            cascade,
            list_items: BudgetVec::new(memory)?,
            marker_count: 0,
            tables: false,
            floats: false,
            inline_blocks: false,
            flex: false,
        };
        let mut numbering = list::Numbering::new(memory)?;
        // List items are admitted while the index and scratch live.
        let list_base = fixed + out.styles.charged_bytes();
        let mut list_bytes = 0usize;
        let mut last = BudgetVec::filled(memory, document.len(), None)?;
        let mut parents = BudgetVec::filled(memory, document.len(), None)?;
        let mut depths = BudgetVec::filled(memory, document.len(), 0usize)?;
        let mut previous_root: Option<BoxId> = None;
        let mut visited = 0usize;
        // Walk actual links; repaired HTML need not follow allocation order.
        let mut next = document.node(document.root()).and_then(|n| n.first_child());
        while let Some(node_id) = next {
            visited = visited.wrapping_add(1);
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
                if kind == BoxKind::Block
                    || (kind == BoxKind::Image
                        && !matches!(
                            keyword(computed, Property::Display),
                            "inline" | "inline-block" | "inline-table" | "inline-flex"
                        ))
                {
                    out.geometry_count = out.geometry_count.checked_add(1).ok_or(Error::Invalid)?;
                }
                let id = BoxId(out.boxes.len());
                out.boxes.try_push(BoxNode {
                    node: node_id,
                    style,
                    kind,
                    parent,
                    first_child: None,
                    next_sibling: None,
                })?;
                if kind == BoxKind::Block {
                    out.tables |= TableRole::of(keyword(computed, Property::Display)).is_some();
                }
                out.floats |= parent.is_some() && float_side(computed, kind).is_some();
                out.inline_blocks |= parent.is_some() && inline_block(computed, kind);
                out.flex |= flex_container(computed, kind);
                if let Some(element) = node.element() {
                    if kind == BoxKind::Block && keyword(computed, Property::Display) == "list-item"
                    {
                        if keyword(computed, Property::ListStyleType) != "none" {
                            out.marker_count =
                                out.marker_count.checked_add(1).ok_or(Error::Invalid)?;
                        }
                        let limit = limits.bytes;
                        numbering.item(id, node.parent(), element, &mut |bytes| {
                            list_bytes = list_bytes
                                .checked_add(bytes)
                                .filter(|n| n.checked_add(list_base).is_some_and(|t| t <= limit))
                                .ok_or(Error::Limit("MAX_PAGE_BYTES"))?;
                            Ok(())
                        })?;
                    }
                    numbering.enter(node_id, element)?;
                }
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
            // After this node's depth and box tests, so they refuse by name.
            if visited.is_multiple_of(CANCEL_INTERVAL) && cancelled(cancel) {
                return Err(Error::Cancelled);
            }
            if descend {
                if let Some(child) = node.first_child() {
                    next = Some(child);
                    continue;
                }
            }
            let mut cursor = node_id;
            loop {
                numbering.leave(cursor)?;
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
        out.list_items = numbering.finish()?;
        out.styled_bytes = out
            .styled_bytes
            .checked_add(list_bytes)
            .ok_or(Error::Invalid)?;
        Ok(out)
    }
    pub fn document(&self) -> &Document {
        match &self.document {
            DocumentOwner::Borrowed(d) => d,
            DocumentOwner::Shared(d) => d,
        }
    }
    pub fn budget(&self) -> &Budget {
        self.boxes.budget()
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
    /// Whether box `index` of `boxes` is laid as a float (DESIGN.md §5,
    /// "Floats"), by the rule layout floats it by: never the root, nor a
    /// table, which keeps its place in flow whatever its `float`.
    pub fn box_floats(&self, index: usize) -> bool {
        let Some(b) = self.boxes.get(index) else {
            return false;
        };
        self.floats
            && b.parent.is_some()
            && self
                .style(b.style)
                .is_some_and(|style| float_side(style, b.kind).is_some())
    }
    /// Whether box `index` of `boxes` is laid whole in its line as an
    /// inline-block or an inline table (DESIGN.md §5, "Inline-blocks"):
    /// never the root.
    pub fn box_is_inline_block(&self, index: usize) -> bool {
        let Some(b) = self.boxes.get(index) else {
            return false;
        };
        self.inline_blocks
            && b.parent.is_some()
            && self
                .style(b.style)
                .is_some_and(|style| inline_block(style, b.kind))
    }
    /// Whether `geometry` was laid for a float (DESIGN.md §5, "Floats"):
    /// never the root's, nor any on a page without floats.
    pub fn is_float(&self, geometry: &crate::block::BoxGeometry) -> bool {
        if !self.floats {
            return false;
        }
        let root = self.first.and_then(|id| self.box_node(id)).map(|b| b.node);
        let kind = if geometry.replaced {
            BoxKind::Image
        } else {
            BoxKind::Block
        };
        root != Some(geometry.node)
            && self
                .style(geometry.style)
                .is_some_and(|style| float_side(style, kind).is_some())
    }
    /// Whether `geometry` was laid for an inline-block or an inline table
    /// (DESIGN.md §5, "Inline-blocks"): never the root's, nor any on a
    /// page without them.
    pub fn is_inline_block(&self, geometry: &crate::block::BoxGeometry) -> bool {
        if !self.inline_blocks || geometry.replaced {
            return false;
        }
        let root = self.first.and_then(|id| self.box_node(id)).map(|b| b.node);
        root != Some(geometry.node)
            && self
                .style(geometry.style)
                .is_some_and(|style| inline_block(style, BoxKind::Block))
    }
    /// Whether `geometry` was laid for a flex container (DESIGN.md §5,
    /// "Flex"), whose items paint as inline-blocks do: none on a page
    /// without one.
    pub fn is_flex_container(&self, geometry: &crate::block::BoxGeometry) -> bool {
        self.flex
            && !geometry.replaced
            && self
                .style(geometry.style)
                .is_some_and(|style| flex_container(style, BoxKind::Block))
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
    /// Its declared charges, by owner: a page's ledger takes `styled`
    /// from each styled document and `tree` once from the document.
    pub fn charges(&self) -> Charges {
        Charges {
            tree: self.document().charged_bytes(),
            styled: self.styled_bytes,
            index: self.index_bytes,
            scratch: self.scratch_bytes,
        }
    }
    /// The clamped limits it was built under, without the build's flag.
    pub fn limits(&self) -> Limits<'static> {
        self.limits
    }
    pub fn cascade_work(&self) -> CascadeWork {
        self.cascade
    }
    /// A list-item box's ordinal value (HTML §4.4.8), clamped to i32; None
    /// for any other box.
    pub fn ordinal(&self, id: BoxId) -> Option<i32> {
        self.list_items
            .binary_search_by_key(&id.0, |item| item.id.0)
            .ok()
            .and_then(|at| self.list_items.get(at))
            .map(|item| item.ordinal)
    }
    /// The number of list-item boxes.
    pub fn list_item_count(&self) -> usize {
        self.list_items.len()
    }
    /// The list items with a marker, which a whole layout keeps each one of.
    pub fn marker_count(&self) -> usize {
        self.marker_count
    }
    pub(crate) fn list_items(&self) -> &[list::ListItem] {
        &self.list_items
    }
    /// A block box's part in a table, by its display: what a consumer
    /// walking box parents needs to tell cells and rows apart.
    pub fn table_role(&self, id: BoxId) -> Option<TableRole> {
        let b = self.boxes.get(id.0)?;
        if b.kind != BoxKind::Block {
            return None;
        }
        TableRole::of(keyword(self.styles.get(b.style)?, Property::Display))
    }
    /// Whether any box has a table display.
    pub fn has_tables(&self) -> bool {
        self.tables
    }
    /// Whether any box floats (DESIGN.md §5, "Floats").
    pub fn has_floats(&self) -> bool {
        self.floats
    }
    /// Whether any box is an inline-block or an inline table (DESIGN.md
    /// §5, "Inline-blocks").
    pub fn has_inline_blocks(&self) -> bool {
        self.inline_blocks
    }
    /// Whether any line may lie within another: a float's or an
    /// inline-block's lines lie within the line holding it.
    pub fn has_nested_lines(&self) -> bool {
        self.floats || self.inline_blocks
    }
    /// Whether any box is a flex container (DESIGN.md §5, "Flex").
    pub fn has_flex(&self) -> bool {
        self.flex
    }
}
impl StyledDocument<'static> {
    /// Own the immutable document without a self-referential borrow.
    pub fn shared(
        document: BudgetShared<Document>,
        sheets: &StyleSet,
        input_bytes: usize,
        limits: Limits<'_>,
        memory: &Budget,
    ) -> Result<Self, Error> {
        let built = StyledDocument::with_budget(&document, sheets, input_bytes, limits, memory)?;
        let StyledDocument {
            styles,
            boxes,
            first,
            geometry_count,
            styled_bytes,
            index_bytes,
            scratch_bytes,
            limits,
            cascade,
            list_items,
            marker_count,
            tables,
            floats,
            inline_blocks,
            flex,
            ..
        } = built;
        Ok(Self {
            document: DocumentOwner::Shared(document),
            styles,
            boxes,
            first,
            geometry_count,
            styled_bytes,
            index_bytes,
            scratch_bytes,
            limits,
            cascade,
            list_items,
            marker_count,
            tables,
            floats,
            inline_blocks,
            flex,
        })
    }
}
/// The side a box of `kind` floats to: a block, or an image laid as a
/// block, whose `float` is left or right and whose display is not a
/// table's, which keeps its place in flow (DESIGN.md §5, "Floats"). The
/// root never floats; its caller sees to that.
pub(crate) fn float_side(style: &ComputedStyle, kind: BoxKind) -> Option<Side> {
    let display = keyword(style, Property::Display);
    let block = kind == BoxKind::Block
        || (kind == BoxKind::Image
            && !matches!(
                display,
                "inline" | "inline-block" | "inline-table" | "inline-flex"
            ));
    if !block || TableRole::of(display).is_some() {
        return None;
    }
    match keyword(style, Property::Float) {
        "left" => Some(Side::Left),
        "right" => Some(Side::Right),
        _ => None,
    }
}
/// Whether a box of `kind` is an inline-block: a block whose display is
/// `inline-block`, `inline-table` or `inline-flex`, laid whole in a line
/// (DESIGN.md §5, "Inline-blocks", "Flex"). td-css blockifies a floated
/// one, so none floats; the root never is one, which its caller sees to.
pub(crate) fn inline_block(style: &ComputedStyle, kind: BoxKind) -> bool {
    kind == BoxKind::Block
        && matches!(
            keyword(style, Property::Display),
            "inline-block" | "inline-table" | "inline-flex"
        )
}
/// Whether a box of `kind` is a flex container: a block whose display is
/// `flex` or `inline-flex`, the latter an inline-block's content
/// (DESIGN.md §5, "Flex").
pub(crate) fn flex_container(style: &ComputedStyle, kind: BoxKind) -> bool {
    kind == BoxKind::Block && matches!(keyword(style, Property::Display), "flex" | "inline-flex")
}
/// The side a float is on, or a clear applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}
fn default_budget(limits: Limits<'_>) -> Result<Budget, Error> {
    Ok(Budget::new(
        limits.bytes.min(MAX_PAGE_BYTES) + WORKING_BYTES + Budget::metadata_bytes(),
    )?)
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
    fn charges_split_by_owner_and_sum_to_what_construction_admits() {
        let html = "<p>one<p>two <b>three</b><div>four</div>";
        let dom = td_html::document::parse(html).unwrap();
        let styled = StyledDocument::new(&dom, &sheets(""), html.len(), Limits::default()).unwrap();
        let charges = styled.charges();
        // The tree's is its document's; the index's is td-dom's per node.
        assert_eq!(charges.tree, dom.charged_bytes());
        assert!(charges.tree > 0);
        assert_eq!(
            charges.index,
            dom.len() * (td_dom::MAX_INDEX_BYTES / td_dom::MAX_INDEX_NODES)
        );
        // Construction's per-node maps are its own scratch; the styled
        // term is what the styled document keeps: input, boxes and pool.
        assert_eq!(
            charges.scratch,
            dom.len() * (size_of::<Option<StyleId>>() + 4 * size_of::<Option<BoxId>>())
        );
        let own = dom.len() * size_of::<BoxNode>() + html.len() + 1024;
        assert_eq!(charges.styled, own + styled.styles.charged_bytes());
        assert_eq!(charges.held(), charges.tree + charges.styled);
        // The four are the single charge construction admitted before.
        assert_eq!(
            charges.construction(),
            dom.len()
                * (td_dom::MAX_INDEX_BYTES / td_dom::MAX_INDEX_NODES
                    + size_of::<Option<StyleId>>()
                    + size_of::<BoxNode>()
                    + 4 * size_of::<Option<BoxId>>())
                + dom.charged_bytes()
                + html.len()
                + 1024
                + styled.styles.charged_bytes()
        );
        // A longer text grows the tree's charge, and the styled term only
        // by the input the caller declares: declared as the short page's,
        // the styled term is unchanged.
        let long = format!("<p>{}<p>two <b>three</b><div>four</div>", "x".repeat(4096));
        let grown = td_html::document::parse(&long).unwrap();
        assert_eq!(grown.len(), dom.len());
        let charge = |input| {
            StyledDocument::new(&grown, &sheets(""), input, Limits::default())
                .unwrap()
                .charges()
        };
        let other = charge(html.len());
        assert!(other.tree > charges.tree);
        assert_eq!(
            (other.styled, other.index, other.scratch),
            (charges.styled, charges.index, charges.scratch)
        );
        assert_eq!(
            charge(long.len()).styled,
            charges.styled + long.len() - html.len()
        );
        // A layout starts from what is held; index and scratch are gone.
        let out =
            block::layout(&styled, &mut Count(0), &block::Request::new(400.0, 300.0)).unwrap();
        assert_eq!(out.page_bytes, charges.held() + out.output_bytes);
    }
    #[test]
    fn a_shared_document_is_charged_once_and_reports_as_a_borrowed_one() {
        use td_alloc::{Budget, BudgetShared};
        let html = "<ol><li>one<li>two</ol><p>three <i>four</i>";
        let css = sheets("p{color:red}");
        let page = Budget::new(16 * 1024 * 1024).unwrap();
        let doc = td_html::document::parse_with_budget(html, Default::default(), &page).unwrap();
        let shared = BudgetShared::new(&page, doc).unwrap();
        let holder = shared.try_clone().unwrap();
        let borrowed =
            StyledDocument::with_budget(&holder, &css, html.len(), Limits::default(), &page)
                .unwrap();
        let alone = borrowed.charges();
        let first = StyledDocument::shared(
            shared.try_clone().unwrap(),
            &css,
            html.len(),
            Limits::default(),
            &page,
        )
        .unwrap();
        let second =
            StyledDocument::shared(shared, &css, html.len(), Limits::default(), &page).unwrap();
        // Borrowed and shared report alike.
        assert_eq!(first.charges(), alone);
        assert_eq!(second.charges(), alone);
        // Each names the one document's charge as its tree, which neither
        // styled term holds: a ledger takes the tree once, beside both.
        assert_eq!(alone.tree, first.document().charged_bytes());
        let ledger = alone.tree + first.charges().styled + second.charges().styled;
        assert_eq!(
            ledger,
            first.charges().held() + second.charges().held() - alone.tree
        );
        assert!(ledger < first.charges().held() + second.charges().held());
        // A shared document with a larger charge and the same nodes, the
        // input declared alike, leaves the styled term as it was.
        let long = format!(
            "<ol><li>one<li>two</ol><p>three <i>{}</i>",
            "y".repeat(4096)
        );
        let grown = td_html::document::parse_with_budget(&long, Default::default(), &page).unwrap();
        assert_eq!(grown.len(), first.document().len());
        let grown = StyledDocument::shared(
            BudgetShared::new(&page, grown).unwrap(),
            &css,
            html.len(),
            Limits::default(),
            &page,
        )
        .unwrap()
        .charges();
        assert!(grown.tree > alone.tree);
        assert_eq!(grown.styled, alone.styled);
        // Their layouts admit alike, each from its own tree and styled.
        let request = block::Request::new(300.0, 200.0);
        let a = block::layout(&borrowed, &mut Count(0), &request).unwrap();
        let b = block::layout(&second, &mut Count(0), &request).unwrap();
        assert_eq!(a.page_bytes, b.page_bytes);
        assert_eq!(a.output_bytes, b.output_bytes);
    }
    #[test]
    fn construction_returns_its_index_and_scratch_to_the_budget() {
        use td_alloc::Budget;
        // A list-free page leaves under a byte a node between what the
        // budget holds and the styled term, so any one per-node map kept
        // (8 bytes a node at the least) passes it; a list's items too.
        let pages = [
            (format!("<body>{}", "<p>x".repeat(20_000)), 1),
            (format!("<ol>{}</ol>", "<li>item".repeat(2000)), 64),
        ];
        for (html, slack) in pages {
            let dom = td_html::document::parse(&html).unwrap();
            assert!(dom.len() > 4000);
            let page = Budget::new(64 * 1024 * 1024).unwrap();
            let child = page.child(32 * 1024 * 1024).unwrap();
            let base = child.used();
            let css = sheets("");
            let styled =
                StyledDocument::with_budget(&dom, &css, html.len(), Limits::default(), &child)
                    .unwrap();
            let used = child.used() - base;
            let charges = styled.charges();
            // The input is the caller's, not in the budget; what remains is
            // the boxes, list items and pool, within the styled term less it.
            let own = charges.styled - html.len() - 1024;
            assert!(used >= styled.boxes.capacity() * size_of::<BoxNode>());
            assert!(used <= own);
            assert!(
                own - used < dom.len() * slack,
                "{} {}",
                own - used,
                dom.len()
            );
            drop(styled);
            assert_eq!(child.used(), base);
        }
    }
    #[test]
    fn page_and_box_limits_at_edges() {
        let dom = td_html::document::parse("<p>x").unwrap();
        let sheet = sheets("");
        let styled = StyledDocument::new(&dom, &sheet, 4, Limits::default()).unwrap();
        let count = styled.boxes().len();
        let bytes = styled.charges().construction();
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
    #[test]
    fn input_limit_is_settable_up_to_four_mib() {
        let dom = td_html::document::parse("<p>x").unwrap();
        let sheet = sheets("");
        let at = |input| Limits {
            input,
            ..Limits::default()
        };
        assert_eq!(Limits::default().input, MAX_PAGE_INPUT);
        assert_eq!(MAX_INPUT, 4 * 1024 * 1024);
        let raised = StyledDocument::new(&dom, &sheet, MAX_INPUT, at(usize::MAX)).unwrap();
        assert_eq!(raised.limits().input, MAX_INPUT);
        assert!(matches!(
            StyledDocument::new(&dom, &sheet, MAX_INPUT + 1, at(usize::MAX)),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
        let between = MAX_PAGE_INPUT + 1;
        assert!(StyledDocument::new(&dom, &sheet, between, at(between)).is_ok());
        assert!(matches!(
            StyledDocument::new(&dom, &sheet, between + 1, at(between)),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
        assert!(StyledDocument::new(&dom, &sheet, 10, at(10)).is_ok());
        assert!(matches!(
            StyledDocument::new(&dom, &sheet, 11, at(10)),
            Err(Error::Limit("MAX_PAGE_INPUT"))
        ));
    }
    /// Each box's node, kind and computed style, in box order.
    fn computed<'a>(styled: &'a StyledDocument<'_>) -> Vec<(usize, BoxKind, &'a ComputedStyle)> {
        styled
            .boxes()
            .iter()
            .map(|b| (b.node.index(), b.kind, styled.style(b.style).unwrap()))
            .collect()
    }
    #[test]
    fn without_style_attributes_none_is_cascaded() {
        let attributed = "<body style='--x:4px;color:red'>\
            <p style='margin-left:var(--x);font-size:30px'>a<span style='display:none'>b</span>\
            <p style='color:green !important'>c<i style=''>d</i><b style='--y:1;font-weight:100'>e";
        let removed = "<body><p>a<span>b</span><p>c<i>d</i><b>e";
        // Another attribute in its place keeps the attribute scans equal.
        let renamed = attributed.replace(" style=", " data-s=");
        let set = sheets("p{color:blue;margin-left:var(--x,1px)}span{font-weight:bold}");
        let off = Limits {
            style_attributes: false,
            ..Limits::default()
        };
        let build = |html: &str, limits| {
            let dom = td_html::document::parse(html).unwrap();
            let styled = StyledDocument::new(&dom, &set, html.len(), limits).unwrap();
            let styles: Vec<_> = computed(&styled)
                .into_iter()
                .map(|(node, kind, style)| (node, kind, style.try_clone().unwrap()))
                .collect();
            (styles, styled.cascade_work(), styled.style_count())
        };
        let (ignored, ignored_work, ignored_count) = build(attributed, off);
        let (bare, bare_work, bare_count) = build(removed, Limits::default());
        let (other, other_work, _) = build(&renamed, Limits::default());
        let (applied, applied_work, _) = build(attributed, Limits::default());
        assert!(!off.clamped().style_attributes);
        assert_eq!(ignored.len(), bare.len());
        for ((_, kind, style), (_, bare_kind, bare_style)) in ignored.iter().zip(&bare) {
            assert_eq!((kind, style), (bare_kind, bare_style));
        }
        assert_eq!(ignored, other);
        assert_eq!(ignored_count, bare_count);
        assert_eq!(ignored_work, other_work);
        assert_eq!(ignored_work.declarations, bare_work.declarations);
        assert_eq!(
            ignored_work.custom_declarations,
            bare_work.custom_declarations
        );
        assert_eq!(
            ignored_work.local_custom_properties,
            bare_work.local_custom_properties
        );
        // Applied, they change styles, boxes and charges.
        assert_ne!(ignored, applied);
        assert!(applied_work.declarations > ignored_work.declarations);
        // An attribute td-css would refuse is never parsed.
        let hostile = format!("<p style='a:{}'>x", "(".repeat(64));
        let dom = td_html::document::parse(&hostile).unwrap();
        assert!(matches!(
            StyledDocument::new(&dom, &set, hostile.len(), Limits::default()),
            Err(Error::Css(_))
        ));
        let styled = StyledDocument::new(&dom, &set, hostile.len(), off).unwrap();
        assert!(!styled.limits().style_attributes);
    }
    fn paragraphs(count: usize) -> String {
        format!("<body>{}", "<p>x</p>".repeat(count))
    }
    fn page_work(set: &StyleSet, count: usize, limits: Limits) -> Result<CascadeWork, Error> {
        let html = paragraphs(count);
        let dom = td_html::document::parse(&html).unwrap();
        StyledDocument::new(&dom, set, html.len(), limits).map(|s| s.cascade_work())
    }
    #[test]
    fn hostile_cascades_meet_the_page_ceiling_by_name() {
        let not: String = (0..2000)
            .map(|i| format!("*:not(.c{i}){{color:red}}"))
            .collect();
        let var = format!(":root{{--a:{}}}*{{color:var(--a)}}", "a ".repeat(30_000));
        // Measured (DESIGN.md §6): each such paragraph costs at least this,
        // so the default ceiling refuses by 8,001 and 427 of them.
        for (css, least, most) in [(not, 16_000, 8_000), (var, 300_000, 427)] {
            let set = sheets(&css);
            let one = page_work(&set, 1, Limits::default()).unwrap();
            let three = page_work(&set, 3, Limits::default()).unwrap();
            let each = (three.total - one.total) / 2;
            assert!(each >= least, "{each}");
            assert!(MAX_CASCADE_WORK / least <= most);
            let at = Limits {
                cascade_work: three.total,
                ..Limits::default()
            };
            assert_eq!(page_work(&set, 3, at), Ok(three));
            assert_eq!(
                page_work(&set, 4, at),
                Err(Error::Limit("MAX_CASCADE_WORK"))
            );
            let below = Limits {
                cascade_work: three.total - 1,
                ..Limits::default()
            };
            assert_eq!(
                page_work(&set, 3, below),
                Err(Error::Limit("MAX_CASCADE_WORK"))
            );
        }
    }
    #[test]
    fn page_like_cascades_stay_far_under_the_ceiling() {
        // A framework's variables on the root, utility classes resolving
        // them, and descendant and structural rules, at real-page density.
        let mut css = String::from(":root{");
        for i in 0..400 {
            css.push_str(&format!("--v{i}:{i}px;"));
        }
        css.push('}');
        for i in 0..1500 {
            css.push_str(&format!(".u{i}{{margin-left:var(--v{})}}", i % 400));
        }
        css.push_str(".content p span,nav a:hover,article>p:first-child,[data-x^=y]{color:red}");
        css.push_str("*{--tw-a:0;--tw-b:0;--tw-c:0;--tw-d:0}");
        let set = sheets(&css);
        let unit = "<div class='content u1'><p class='u2 u3 u4' data-x=yes>text \
                    <span class=u5>x</span><a href=#>link</a></p></div>";
        let html = format!("<body>{}", unit.repeat(1000));
        let dom = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&dom, &set, html.len(), Limits::default()).unwrap();
        let work = styled.cascade_work();
        assert_eq!(work.elements, 4003);
        assert_eq!(work.local_custom_properties, 4 * 4003 + 400);
        // Scaled to a whole MAX_PAGE_INPUT page of the same markup.
        let scaled = work.total.saturating_mul(MAX_PAGE_INPUT) / html.len();
        assert!(scaled * 2 < MAX_CASCADE_WORK, "{work:?}");
    }
    #[test]
    fn inherited_map_copies_and_attribute_scans_are_counted() {
        // An element whose own name differs from its parent's copies the
        // inherited map, even when display:none keeps it out of the pool.
        let names: String = (0..1000).map(|i| format!("--a{i:04}:0;")).collect();
        let set = sheets(&format!(
            ":root{{{names}}}body{{--x:2}}p{{--x:1}}p{{display:none}}"
        ));
        let one = page_work(&set, 1, Limits::default()).unwrap();
        let three = page_work(&set, 3, Limits::default()).unwrap();
        assert_eq!(three.custom_copies - one.custom_copies, 2 * 1000);
        assert!((three.total - one.total) / 2 >= 6 * 1000);
        // A lookup among 256 attributes costs 64 steps.
        let attributes: String = (0..256).map(|i| format!(" x{i:03}")).collect();
        let set = sheets("[x255]{color:red}");
        let work = |count: usize| {
            let html = format!("<body>{}", format!("<p{attributes}>").repeat(count));
            let dom = td_html::document::parse(&html).unwrap();
            StyledDocument::new(&dom, &set, html.len(), Limits::default())
                .unwrap()
                .cascade_work()
        };
        let (one, three) = (work(1), work(3));
        assert!((three.match_steps - one.match_steps) / 2 >= 64);
    }
    #[test]
    fn reference_and_declaration_order_cost_the_same() {
        // td-css indexes a cascade's custom names by position (DESIGN.md §6),
        // so order changes no charge, and every shape is charged its size.
        let each = |css: &str| {
            let set = sheets(css);
            let one = page_work(&set, 1, Limits::default()).unwrap();
            let three = page_work(&set, 3, Limits::default()).unwrap();
            (three.total - one.total) / 2
        };
        let n = 4000;
        let refs = |name: &dyn Fn(usize) -> usize| {
            let refs: String = (0..n).map(|i| format!("var(--z{:05})", name(i))).collect();
            format!("p{{--x:{refs}}}")
        };
        let descending = each(&refs(&|i| n - i));
        assert_eq!(descending, each(&refs(&|i| i + 1)));
        assert!(descending >= 5 * 2 * n);
        // A name referring to later names, with names between, against the
        // same graph with the referring name sorting last.
        let graph = |first: &str| {
            let refs: String = (0..1000).map(|i| format!("var(--c{i:04})")).collect();
            let between: String = (0..999).map(|i| format!("--b{i:04}:1;")).collect();
            let later: String = (0..1000).map(|i| format!("--c{i:04}:1;")).collect();
            format!("*{{{first}:{refs};{between}{later}}}")
        };
        let early = each(&graph("--a0000"));
        assert_eq!(early, each(&graph("--z0000")));
        assert!(early >= 6 * 3000);
    }
    #[test]
    fn cascade_ceiling_is_settable_and_clamped() {
        let set = sheets("");
        let work = page_work(&set, 2, Limits::default()).unwrap();
        assert!(work.total > 0 && work.elements == 5);
        let at = |cascade_work| Limits {
            cascade_work,
            ..Limits::default()
        };
        assert_eq!(page_work(&set, 2, at(work.total)), Ok(work));
        assert_eq!(
            page_work(&set, 2, at(work.total - 1)),
            Err(Error::Limit("MAX_CASCADE_WORK"))
        );
        let html = paragraphs(2);
        let dom = td_html::document::parse(&html).unwrap();
        let styled = StyledDocument::new(&dom, &set, html.len(), at(usize::MAX)).unwrap();
        assert_eq!(styled.limits().cascade_work, MAX_CASCADE_WORK);
    }
    /// Asks another thread to set the flag at its `at`th call, then counts.
    struct Trip<'a> {
        at: usize,
        calls: std::cell::Cell<usize>,
        ask: std::sync::mpsc::Sender<()>,
        done: std::sync::mpsc::Receiver<()>,
        flag: &'a AtomicBool,
    }
    impl Trip<'_> {
        fn call(&self) {
            let calls = self.calls.get() + 1;
            self.calls.set(calls);
            if calls == self.at {
                self.ask.send(()).unwrap();
                self.done.recv().unwrap();
                assert!(self.flag.load(Ordering::Relaxed));
            }
        }
        fn after(&self) -> usize {
            self.calls.get() - self.at
        }
    }
    impl StateProvider for Trip<'_> {
        fn state(&self, _: NodeId, _: td_css::selectors::State) -> bool {
            self.call();
            false
        }
    }
    impl inline::FontMetrics for Trip<'_> {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            self.call();
            512
        }
        fn line(&mut self, _: &ComputedStyle) -> inline::FontLine {
            inline::FontLine {
                height: 1280,
                baseline: 960,
            }
        }
    }
    /// Runs `job` with a Trip whose flag a second thread sets.
    fn tripped<R>(at: usize, job: impl FnOnce(&mut Trip<'_>, &AtomicBool) -> R) -> (R, usize) {
        let flag = AtomicBool::new(false);
        let (ask, asked) = std::sync::mpsc::channel();
        let (reply, done) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let flag = &flag;
            scope.spawn(move || {
                if asked.recv().is_ok() {
                    flag.store(true, Ordering::Relaxed);
                    reply.send(()).unwrap();
                }
            });
            let mut trip = Trip {
                at,
                calls: std::cell::Cell::new(0),
                ask,
                done,
                flag,
            };
            let result = job(&mut trip, flag);
            let after = trip.after();
            (result, after)
        })
    }
    #[test]
    fn cancelling_mid_cascade_stops_at_the_next_element_and_returns_charges() {
        let html = paragraphs(2000);
        let dom = td_html::document::parse(&html).unwrap();
        let set = sheets("p:hover{color:red}");
        let memory = Budget::new(64 * 1024 * 1024).unwrap();
        let base = memory.used();
        let (result, after) = tripped(1000, |trip, flag| {
            let limits = Limits {
                cancel: Some(flag),
                ..Limits::default()
            };
            StyledDocument::with_state_and_budget(&dom, &set, html.len(), limits, trip, &memory)
                .map(|s| s.boxes().len())
        });
        assert_eq!(result, Err(Error::Cancelled));
        // The element being computed when the flag was set finishes; no other.
        assert_eq!(after, 0);
        assert_eq!(memory.used(), base);
    }
    #[test]
    fn cancelling_mid_layout_stops_within_an_interval_and_returns_charges() {
        let html = format!("<p>{}", "words here ".repeat(20_000));
        let dom = td_html::document::parse(&html).unwrap();
        let set = sheets("");
        let memory = Budget::new(64 * 1024 * 1024).unwrap();
        let styled =
            StyledDocument::with_budget(&dom, &set, html.len(), Limits::default(), &memory)
                .unwrap();
        let retained = memory.used();
        let (result, after) = tripped(50_000, |trip, flag| {
            let request = block::Request {
                cancel: Some(flag),
                ..block::Request::new(400.0, 300.0)
            };
            block::layout(&styled, trip, &request).map(|l| l.line_count)
        });
        assert_eq!(result.err(), Some(Error::Cancelled));
        assert!(after <= CANCEL_INTERVAL, "{after}");
        assert_eq!(memory.used(), retained);
    }
    #[test]
    fn a_flag_set_before_the_call_cancels_at_once() {
        let html = paragraphs(4);
        let dom = td_html::document::parse(&html).unwrap();
        let set = sheets("");
        let memory = Budget::new(16 * 1024 * 1024).unwrap();
        let base = memory.used();
        let set_flag = AtomicBool::new(true);
        let limits = Limits {
            cancel: Some(&set_flag),
            ..Limits::default()
        };
        assert!(matches!(
            StyledDocument::with_budget(&dom, &set, html.len(), limits, &memory),
            Err(Error::Cancelled)
        ));
        assert_eq!(memory.used(), base);
        let styled =
            StyledDocument::with_budget(&dom, &set, html.len(), Limits::default(), &memory)
                .unwrap();
        let retained = memory.used();
        let request = block::Request {
            cancel: Some(&set_flag),
            ..block::Request::new(400.0, 300.0)
        };
        let mut metrics = Count(0);
        assert!(matches!(
            block::layout(&styled, &mut metrics, &request),
            Err(Error::Cancelled)
        ));
        assert_eq!(metrics.0, 0);
        assert_eq!(memory.used(), retained);
    }
    #[test]
    fn up_front_limits_refuse_by_name_before_a_set_flag() {
        let html = paragraphs(4);
        let dom = td_html::document::parse(&html).unwrap();
        let set = sheets("");
        let set_flag = AtomicBool::new(true);
        let flagged = Limits {
            cancel: Some(&set_flag),
            ..Limits::default()
        };
        let styled = StyledDocument::new(&dom, &set, html.len(), Limits::default()).unwrap();
        for (limits, input, name) in [
            (flagged, MAX_PAGE_INPUT + 1, "MAX_PAGE_INPUT"),
            (
                // Under the fixed arrays' charge, made before the pool's.
                Limits {
                    bytes: 1024,
                    ..flagged
                },
                html.len(),
                "MAX_PAGE_BYTES",
            ),
        ] {
            assert!(matches!(
                StyledDocument::new(&dom, &set, input, limits),
                Err(Error::Limit(n)) if n == name
            ));
        }
        let mut request = block::Request {
            cancel: Some(&set_flag),
            ..block::Request::new(400.0, 300.0)
        };
        // What the styled document and its tree hold, then that and the
        // geometry array's charge, then the retained output's.
        for (bytes, retained) in [
            (Some(styled.charges().held() - 1), 0),
            (Some(styled.charges().held()), 0),
            (None, MAX_PAGE_BYTES),
        ] {
            request.bytes = bytes;
            request.retained_output_bytes = retained;
            let mut metrics = Count(0);
            assert!(matches!(
                block::layout(&styled, &mut metrics, &request),
                Err(Error::Limit("MAX_PAGE_BYTES"))
            ));
            assert_eq!(metrics.0, 0);
        }
    }
    struct Count(usize);
    impl inline::FontMetrics for Count {
        fn advance(&mut self, _: &ComputedStyle, _: char) -> u32 {
            self.0 += 1;
            512
        }
        fn line(&mut self, _: &ComputedStyle) -> inline::FontLine {
            inline::FontLine {
                height: 1280,
                baseline: 960,
            }
        }
    }
    #[test]
    fn an_unset_flag_changes_nothing() {
        let html = "<p>alpha <b>beta</b> gamma<p>delta epsilon zeta";
        let dom = td_html::document::parse(html).unwrap();
        let set = sheets("b{font-size:20px}");
        let unset = AtomicBool::new(false);
        let with = Limits {
            cancel: Some(&unset),
            ..Limits::default()
        };
        let a = StyledDocument::new(&dom, &set, html.len(), Limits::default()).unwrap();
        let b = StyledDocument::new(&dom, &set, html.len(), with).unwrap();
        assert_eq!(a.cascade_work(), b.cascade_work());
        assert_eq!(a.boxes().len(), b.boxes().len());
        let request = block::Request {
            cancel: Some(&unset),
            ..block::Request::new(60.0, 300.0)
        };
        let plain = block::layout(&a, &mut Count(0), &block::Request::new(60.0, 300.0)).unwrap();
        let flagged = block::layout(&b, &mut Count(0), &request).unwrap();
        assert_eq!(plain.lines.len(), flagged.lines.len());
        assert!(plain.lines.iter().eq(flagged.lines.iter()));
        assert_eq!(
            plain.boxes.iter().collect::<Vec<_>>(),
            flagged.boxes.iter().collect::<Vec<_>>()
        );
        assert_eq!(plain.work, flagged.work);
    }
}

#[cfg(test)]
mod validation;
