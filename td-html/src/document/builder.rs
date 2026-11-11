use super::*;
use crate::html5::{Tag, TextMode, BYTE_CHARGE, NODE_CHARGE};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    Head,
    HeadNoscript,
    AfterHead,
    Body,
    Text,
    Table,
    TableText,
    Caption,
    ColumnGroup,
    TableBody,
    Row,
    Cell,
    Select,
    SelectTable,
    Template,
    AfterBody,
    AfterAfterBody,
}
pub(super) struct Builder<'a> {
    pub tok: Tokenizer<'a>,
    pub doc: Document,
    pub open: Vec<NodeId>,
    pub active: Vec<Option<NodeId>>,
    pub templates: Vec<Mode>,
    pub mode: Mode,
    pub original: Mode,
    pub table_original: Mode,
    pub pending_table: String,
    pub foster: bool,
    pub head: Option<NodeId>,
    pub form: Option<NodeId>,
    pub ignore_lf: bool,
}
impl<'a> Builder<'a> {
    pub fn new(input: &'a str, limits: ParseLimits) -> Result<Self, ParseError> {
        let tok = Tokenizer::new(input, limits)?;
        let root = NodeId::new(0)?;
        let mut b = Self {
            tok,
            doc: Document {
                nodes: Vec::new(),
                root,
                quirks: QuirksMode::NoQuirks,
                errors: 0,
                work: 0,
                charged_bytes: 0,
            },
            open: Vec::new(),
            active: Vec::new(),
            templates: Vec::new(),
            mode: Mode::Initial,
            original: Mode::Body,
            table_original: Mode::Table,
            pending_table: String::new(),
            foster: false,
            head: None,
            form: None,
            ignore_lf: false,
        };
        b.allocate(NodeKind::Document)?;
        Ok(b)
    }
    pub fn parse(mut self) -> Result<Document, ParseError> {
        while let Some(token) = self.tok.next_token()? {
            if let Token::Text(s) = token {
                for c in s.chars() {
                    if self.ignore_lf {
                        self.ignore_lf = false;
                        if c == '\n' {
                            continue;
                        }
                    }
                    self.character(c)?;
                }
            } else {
                self.ignore_lf = false;
                self.flush_table()?;
                self.process(&token)?;
            }
            self.tok.set_foreign(
                self.current_element()
                    .is_some_and(|e| e.namespace != Namespace::Html),
            );
        }
        self.flush_table()?;
        loop {
            self.step()?;
            match self.mode {
                Mode::Initial => {
                    self.doc.quirks = QuirksMode::Quirks;
                    self.mode = Mode::BeforeHtml;
                }
                Mode::BeforeHtml => {
                    self.insert_named("html")?;
                    self.mode = Mode::BeforeHead;
                }
                Mode::BeforeHead => {
                    self.head = Some(self.insert_named("head")?);
                    self.mode = Mode::Head;
                }
                Mode::Head | Mode::HeadNoscript => {
                    self.open.pop();
                    self.mode = if self.mode == Mode::Head {
                        Mode::AfterHead
                    } else {
                        Mode::Head
                    };
                }
                Mode::AfterHead => {
                    self.insert_named("body")?;
                    self.mode = Mode::Body;
                }
                Mode::Text => {
                    self.error();
                    self.open.pop();
                    self.mode = self.original;
                }
                _ if !self.templates.is_empty() => {
                    self.close_template()?;
                }
                _ => break,
            }
        }
        self.doc.errors = self.doc.errors.saturating_add(self.tok.errors());
        self.doc.work = self.tok.work();
        self.doc.charged_bytes = self.tok.charged_bytes();
        Ok(self.doc)
    }
    pub fn step(&mut self) -> Result<(), ParseError> {
        self.tok.budget.step()
    }
    pub fn error(&mut self) {
        self.doc.errors = self.doc.errors.saturating_add(1);
    }
    pub fn node(&self, id: NodeId) -> Result<&Node, ParseError> {
        self.doc.node(id).ok_or(ParseError::State)
    }
    pub fn node_mut(&mut self, id: NodeId) -> Result<&mut Node, ParseError> {
        self.doc.nodes.get_mut(id.index()).ok_or(ParseError::State)
    }
    pub fn current(&self) -> NodeId {
        self.open.last().copied().unwrap_or(self.doc.root)
    }
    pub fn current_element(&self) -> Option<&Element> {
        self.doc.node(self.current()).and_then(Node::element)
    }
    pub fn is(&self, id: NodeId, name: &str) -> bool {
        self.doc
            .node(id)
            .and_then(Node::element)
            .is_some_and(|e| e.namespace == Namespace::Html && e.name == name)
    }
    pub fn current_is(&self, name: &str) -> bool {
        self.is(self.current(), name)
    }
    pub fn allocate(&mut self, kind: NodeKind) -> Result<NodeId, ParseError> {
        if self.doc.nodes.len() >= self.tok.budget.limits.nodes {
            return Err(ParseError::Nodes);
        }
        self.tok.budget.charge(NODE_CHARGE)?;
        let id = NodeId::new(self.doc.nodes.len())?;
        self.doc.nodes.push(Node {
            kind,
            parent: None,
            first: None,
            last: None,
            previous: None,
            next: None,
        });
        Ok(id)
    }
    pub fn owned(&mut self, s: &str) -> Result<String, ParseError> {
        self.tok
            .budget
            .charge(s.len().saturating_mul(BYTE_CHARGE))?;
        Ok(s.to_owned())
    }
    pub fn depth(&mut self, id: NodeId) -> Result<usize, ParseError> {
        let mut n = 0;
        let mut current = Some(id);
        while let Some(id) = current {
            self.step()?;
            current = self.node(id)?.parent;
            if current.is_some() {
                n += 1;
            }
            if n > self.tok.budget.limits.depth {
                return Err(ParseError::Depth);
            }
        }
        Ok(n)
    }
    pub fn attach(
        &mut self,
        id: NodeId,
        parent: NodeId,
        before: Option<NodeId>,
    ) -> Result<(), ParseError> {
        let mut ancestor = Some(parent);
        while let Some(a) = ancestor {
            self.step()?;
            if a == id {
                return Err(ParseError::State);
            }
            ancestor = self.node(a)?.parent;
        }
        let base = self.depth(parent)?.saturating_add(1);
        let mut cursor = id;
        let mut depth = base;
        loop {
            self.step()?;
            if depth > self.tok.budget.limits.depth {
                return Err(ParseError::Depth);
            }
            if let Some(child) = self.node(cursor)?.first {
                cursor = child;
                depth += 1;
                continue;
            }
            loop {
                if cursor == id {
                    break;
                }
                if let Some(next) = self.node(cursor)?.next {
                    cursor = next;
                    break;
                }
                cursor = self.node(cursor)?.parent.ok_or(ParseError::State)?;
                depth = depth.saturating_sub(1);
            }
            if cursor == id {
                break;
            }
        }
        if let Some(before) = before {
            if self.node(before)?.parent != Some(parent) || before == id {
                return Err(ParseError::State);
            }
        }
        self.detach(id)?;
        let previous = if let Some(before) = before {
            self.node(before)?.previous
        } else {
            self.node(parent)?.last
        };
        {
            let n = self.node_mut(id)?;
            n.parent = Some(parent);
            n.previous = previous;
            n.next = before;
        }
        if let Some(previous) = previous {
            self.node_mut(previous)?.next = Some(id);
        } else {
            self.node_mut(parent)?.first = Some(id);
        }
        if let Some(before) = before {
            self.node_mut(before)?.previous = Some(id);
        } else {
            self.node_mut(parent)?.last = Some(id);
        }
        Ok(())
    }
    pub fn detach(&mut self, id: NodeId) -> Result<(), ParseError> {
        let n = self.node(id)?;
        let (parent, previous, next) = (n.parent, n.previous, n.next);
        if let Some(parent) = parent {
            if let Some(p) = previous {
                self.node_mut(p)?.next = next;
            } else {
                self.node_mut(parent)?.first = next;
            }
            if let Some(n) = next {
                self.node_mut(n)?.previous = previous;
            } else {
                self.node_mut(parent)?.last = previous;
            }
        }
        let n = self.node_mut(id)?;
        n.parent = None;
        n.previous = None;
        n.next = None;
        Ok(())
    }
    pub fn location(
        &mut self,
        target: Option<NodeId>,
    ) -> Result<(NodeId, Option<NodeId>), ParseError> {
        let mut parent = target.unwrap_or_else(|| self.current());
        let mut before = None;
        if self.foster
            && ["table", "tbody", "tfoot", "thead", "tr"]
                .iter()
                .any(|n| self.is(parent, n))
        {
            let mut table = None;
            let mut template = None;
            for i in (0..self.open.len()).rev() {
                self.step()?;
                let id = *self.open.get(i).ok_or(ParseError::State)?;
                if template.is_none() && self.is(id, "template") {
                    template = Some((i, id));
                }
                if table.is_none() && self.is(id, "table") {
                    table = Some((i, id));
                }
            }
            if let Some((ti, id)) = template.filter(|(ti, _)| table.is_none_or(|(i, _)| *ti > i)) {
                let _ = ti;
                parent = id;
            } else if let Some((i, id)) = table {
                if let Some(p) = self.node(id)?.parent {
                    parent = p;
                    before = Some(id);
                } else {
                    parent = *self
                        .open
                        .get(i.saturating_sub(1))
                        .ok_or(ParseError::State)?;
                }
            } else {
                parent = self.open.first().copied().unwrap_or(self.doc.root);
            }
        }
        if let Some(fragment) = self
            .node(parent)?
            .element()
            .and_then(|e| e.template_contents)
        {
            parent = fragment;
        }
        Ok((parent, before))
    }
    pub fn push_open(&mut self, id: NodeId) -> Result<(), ParseError> {
        if self.open.len() >= self.tok.budget.limits.depth {
            return Err(ParseError::Depth);
        }
        self.open.push(id);
        Ok(())
    }
    pub fn new_element(&mut self, tag: &Tag, namespace: Namespace) -> Result<NodeId, ParseError> {
        let name = self.owned(super::foreign::element_name(&tag.name, namespace))?;
        let mut html_integration = namespace == Namespace::Svg
            && matches!(name.as_str(), "foreignObject" | "desc" | "title");
        let mut attributes = Vec::new();
        for a in &tag.attributes {
            self.step()?;
            if namespace == Namespace::MathMl && name == "annotation-xml" && a.name == "encoding" {
                html_integration = a.value.eq_ignore_ascii_case("text/html")
                    || a.value.eq_ignore_ascii_case("application/xhtml+xml");
            }
            attributes.push(self.convert_attribute(a, namespace)?);
        }
        self.allocate(NodeKind::Element(Element {
            name,
            namespace,
            attributes,
            template_contents: None,
            html_integration,
        }))
    }
    pub fn insert(
        &mut self,
        tag: &Tag,
        namespace: Namespace,
        push: bool,
    ) -> Result<NodeId, ParseError> {
        let id = self.new_element(tag, namespace)?;
        let (parent, before) = self.location(None)?;
        self.attach(id, parent, before)?;
        if push {
            self.push_open(id)?;
        }
        Ok(id)
    }
    pub fn insert_named(&mut self, name: &str) -> Result<NodeId, ParseError> {
        let tag = self.synthetic(name, false)?;
        self.insert(&tag, Namespace::Html, true)
    }
    pub fn synthetic(&mut self, name: &str, end: bool) -> Result<Tag, ParseError> {
        Ok(Tag {
            name: self.owned(name)?,
            attributes: Vec::new(),
            self_closing: false,
            end,
        })
    }
    pub fn merge_attributes(&mut self, id: NodeId, tag: &Tag) -> Result<(), ParseError> {
        for a in &tag.attributes {
            let count = self
                .node(id)?
                .element()
                .ok_or(ParseError::State)?
                .attributes
                .len();
            let mut found = false;
            for i in 0..count {
                self.step()?;
                if self
                    .node(id)?
                    .element()
                    .and_then(|e| e.attributes.get(i))
                    .is_some_and(|old| {
                        old.namespace == AttributeNamespace::None && old.name == a.name
                    })
                {
                    found = true;
                    break;
                }
            }
            if found {
                continue;
            }
            if count >= self.tok.budget.limits.attributes_per_tag {
                return Err(ParseError::AttributesPerTag);
            }
            let attr = self.convert_attribute(a, Namespace::Html)?;
            if let NodeKind::Element(e) = &mut self.node_mut(id)?.kind {
                e.attributes.push(attr);
            }
        }
        Ok(())
    }
    pub fn text(&mut self, c: char) -> Result<(), ParseError> {
        let (parent, before) = self.location(None)?;
        let previous = if let Some(before) = before {
            self.node(before)?.previous
        } else {
            self.node(parent)?.last
        };
        self.tok.budget.charge(c.len_utf8() * BYTE_CHARGE)?;
        if let Some(previous) = previous {
            if let NodeKind::Text(s) = &mut self.node_mut(previous)?.kind {
                s.push(c);
                return Ok(());
            }
        }
        let mut s = String::new();
        s.push(c);
        let id = self.allocate(NodeKind::Text(s))?;
        self.attach(id, parent, before)
    }
    pub fn comment(&mut self, s: &str, parent: NodeId) -> Result<(), ParseError> {
        let parent = self
            .node(parent)?
            .element()
            .and_then(|e| e.template_contents)
            .unwrap_or(parent);
        let text = self.owned(s)?;
        let id = self.allocate(NodeKind::Comment(text))?;
        self.attach(id, parent, None)
    }
    pub fn open_position(&mut self, id: NodeId) -> Result<Option<usize>, ParseError> {
        for i in (0..self.open.len()).rev() {
            self.step()?;
            if self.open.get(i) == Some(&id) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }
    pub fn find_open(&mut self, name: &str) -> Result<Option<usize>, ParseError> {
        for i in (0..self.open.len()).rev() {
            self.step()?;
            if self.open.get(i).is_some_and(|id| self.is(*id, name)) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }
    pub fn pop_through(&mut self, name: &str) -> Result<(), ParseError> {
        if let Some(i) = self.find_open(name)? {
            self.open.truncate(i);
        }
        Ok(())
    }
    pub fn scoped(&mut self, name: &str, kind: &str) -> Result<bool, ParseError> {
        for i in (0..self.open.len()).rev() {
            self.step()?;
            let id = *self.open.get(i).ok_or(ParseError::State)?;
            if self.is(id, name) {
                return Ok(true);
            }
            let e = self.node(id)?.element().ok_or(ParseError::State)?;
            if kind == "select" {
                if e.namespace != Namespace::Html
                    || !matches!(e.name.as_str(), "option" | "optgroup")
                {
                    return Ok(false);
                }
                continue;
            }
            let stop = if kind == "table" {
                e.namespace == Namespace::Html
                    && matches!(e.name.as_str(), "html" | "table" | "template")
            } else {
                super::foreign::scope_boundary(e)
                    || (kind == "button" && self.is(id, "button"))
                    || (kind == "list" && (self.is(id, "ol") || self.is(id, "ul")))
            };
            if stop {
                return Ok(false);
            }
        }
        Ok(false)
    }
    pub fn implied(&mut self, except: &str, thorough: bool) -> Result<(), ParseError> {
        loop {
            self.step()?;
            let pop = self.current_element().is_some_and(|e| {
                e.namespace == Namespace::Html
                    && e.name != except
                    && (matches!(
                        e.name.as_str(),
                        "dd" | "dt"
                            | "li"
                            | "optgroup"
                            | "option"
                            | "p"
                            | "rb"
                            | "rp"
                            | "rt"
                            | "rtc"
                    ) || (thorough
                        && matches!(
                            e.name.as_str(),
                            "caption"
                                | "colgroup"
                                | "tbody"
                                | "td"
                                | "tfoot"
                                | "th"
                                | "thead"
                                | "tr"
                        )))
            });
            if !pop {
                break;
            }
            self.open.pop();
        }
        Ok(())
    }
    pub fn close_p(&mut self) -> Result<(), ParseError> {
        if !self.scoped("p", "button")? {
            self.insert_named("p")?;
        }
        self.implied("p", false)?;
        self.pop_through("p")
    }
    pub fn reset_mode(&mut self) -> Result<(), ParseError> {
        for i in (0..self.open.len()).rev() {
            self.step()?;
            let id = *self.open.get(i).ok_or(ParseError::State)?;
            let Some(e) = self.node(id)?.element() else {
                continue;
            };
            if e.namespace != Namespace::Html {
                continue;
            }
            let mode = match e.name.as_str() {
                "select" => {
                    let mut table = false;
                    for j in (0..i).rev() {
                        self.step()?;
                        let id = *self.open.get(j).ok_or(ParseError::State)?;
                        if self.is(id, "template") {
                            break;
                        }
                        if self.is(id, "table") {
                            table = true;
                            break;
                        }
                    }
                    if table {
                        Mode::SelectTable
                    } else {
                        Mode::Select
                    }
                }
                "td" | "th" => Mode::Cell,
                "tr" => Mode::Row,
                "tbody" | "thead" | "tfoot" => Mode::TableBody,
                "caption" => Mode::Caption,
                "colgroup" => Mode::ColumnGroup,
                "table" => Mode::Table,
                "template" => self.templates.last().copied().unwrap_or(Mode::Template),
                "head" => Mode::Head,
                "body" => Mode::Body,
                "html" => {
                    if self.head.is_some() {
                        Mode::AfterHead
                    } else {
                        Mode::BeforeHead
                    }
                }
                _ => continue,
            };
            self.mode = mode;
            return Ok(());
        }
        self.mode = Mode::Body;
        Ok(())
    }
    pub fn clear_to(&mut self, names: &[&str]) -> Result<(), ParseError> {
        loop {
            self.step()?;
            if self.open.is_empty() || names.iter().any(|n| self.current_is(n)) {
                return Ok(());
            }
            self.open.pop();
        }
    }
    pub fn clear_active(&mut self) -> Result<(), ParseError> {
        while let Some(x) = self.active.pop() {
            self.step()?;
            if x.is_none() {
                break;
            }
        }
        Ok(())
    }
    pub fn close_template(&mut self) -> Result<(), ParseError> {
        if self.find_open("template")?.is_none() {
            self.error();
            return Ok(());
        }
        self.implied("", true)?;
        self.pop_through("template")?;
        self.clear_active()?;
        self.templates.pop();
        self.reset_mode()
    }
    pub fn start_text(&mut self, tag: &Tag, mode: TextMode) -> Result<(), ParseError> {
        self.insert(tag, Namespace::Html, true)?;
        self.tok.set_text_mode(mode);
        self.original = self.mode;
        self.mode = Mode::Text;
        Ok(())
    }
    pub fn flush_table(&mut self) -> Result<(), ParseError> {
        if self.mode != Mode::TableText {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending_table);
        self.mode = self.table_original;
        if pending.chars().all(super::ws) {
            for c in pending.chars() {
                self.text(c)?;
            }
        } else {
            self.error();
            self.foster = true;
            for c in pending.chars() {
                self.body_character(c)?;
            }
            self.foster = false;
        }
        Ok(())
    }
}
