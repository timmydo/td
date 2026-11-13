use super::builder::{Builder, Mode};
use super::*;
use crate::html5::{Tag, TextMode, BYTE_CHARGE};
impl Builder<'_> {
    pub(super) fn character(&mut self, c: char) -> Result<(), ParseError> {
        if self.foreign_text() {
            let c = if c == '\0' {
                self.error();
                '\u{fffd}'
            } else {
                c
            };
            return self.text(c);
        }
        loop {
            self.step()?;
            match self.mode {
                Mode::Initial => {
                    if ws(c) {
                        return Ok(());
                    }
                    self.doc.quirks = QuirksMode::Quirks;
                    self.mode = Mode::BeforeHtml;
                }
                Mode::BeforeHtml => {
                    if ws(c) {
                        return Ok(());
                    }
                    self.insert_named("html")?;
                    self.mode = Mode::BeforeHead;
                }
                Mode::BeforeHead => {
                    if ws(c) {
                        return Ok(());
                    }
                    self.head = Some(self.insert_named("head")?);
                    self.mode = Mode::Head;
                }
                Mode::Head => {
                    if ws(c) {
                        return self.text(c);
                    }
                    self.open.pop();
                    self.mode = Mode::AfterHead;
                }
                Mode::HeadNoscript => {
                    if ws(c) {
                        return self.text(c);
                    }
                    self.error();
                    self.open.pop();
                    self.mode = Mode::Head;
                }
                Mode::AfterHead => {
                    if ws(c) {
                        return self.text(c);
                    }
                    self.insert_named("body")?;
                    self.mode = Mode::Body;
                }
                Mode::Body | Mode::Caption | Mode::Cell => return self.body_character(c),
                Mode::Text => return self.text(c),
                Mode::Table | Mode::TableBody | Mode::Row => {
                    if ["table", "tbody", "tfoot", "thead", "tr"]
                        .iter()
                        .any(|n| self.current_is(n))
                    {
                        self.table_original = self.mode;
                        self.mode = Mode::TableText;
                    } else {
                        self.foster = true;
                        let result = self.body_character(c);
                        self.foster = false;
                        return result;
                    }
                }
                Mode::TableText => {
                    if c == '\0' {
                        self.error();
                        return Ok(());
                    }
                    self.tok.budget.charge(c.len_utf8() * BYTE_CHARGE)?;
                    self.pending_table.try_push(c)?;
                    return Ok(());
                }
                Mode::ColumnGroup => {
                    if ws(c) {
                        return self.text(c);
                    }
                    if !self.current_is("colgroup") {
                        self.error();
                        return Ok(());
                    }
                    self.open.pop();
                    self.mode = Mode::Table;
                }
                Mode::Select | Mode::SelectTable => {
                    if c == '\0' {
                        self.error();
                        return Ok(());
                    }
                    return self.text(c);
                }
                Mode::Template => return self.body_character(c),
                Mode::AfterBody | Mode::AfterAfterBody => {
                    if ws(c) {
                        return self.body_character(c);
                    }
                    self.error();
                    self.mode = Mode::Body;
                }
            }
        }
    }
    pub(super) fn body_character(&mut self, c: char) -> Result<(), ParseError> {
        if c == '\0' {
            self.error();
            return Ok(());
        }
        self.reconstruct()?;
        self.text(c)
    }
    pub(super) fn process(&mut self, token: &Token) -> Result<(), ParseError> {
        if self.foreign_token(token)? {
            return Ok(());
        }
        if let Token::Tag(t) = token {
            if matches!(t.name.as_str(), "frameset" | "frame") {
                self.error();
                return Ok(());
            }
        }
        loop {
            self.step()?;
            match self.mode {
                Mode::Initial => match token {
                    Token::Comment(s) => return self.comment(s, self.doc.root),
                    Token::Doctype(d) => {
                        let d = html5::Doctype {
                            name: self.owned(&d.name)?,
                            public_id: d.public_id.as_deref().map(|s| self.owned(s)).transpose()?,
                            system_id: d.system_id.as_deref().map(|s| self.owned(s)).transpose()?,
                            force_quirks: d.force_quirks,
                        };
                        self.doc.quirks = quirks(&d);
                        let id = self.allocate(NodeKind::Doctype(BudgetShared::new(
                            &self.tok.budget.memory,
                            d,
                        )?))?;
                        self.attach(id, self.doc.root, None)?;
                        self.mode = Mode::BeforeHtml;
                        return Ok(());
                    }
                    _ => {
                        self.doc.quirks = QuirksMode::Quirks;
                        self.mode = Mode::BeforeHtml;
                    }
                },
                Mode::BeforeHtml => match token {
                    Token::Comment(s) => return self.comment(s, self.doc.root),
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => {
                        self.insert_element(t, Namespace::Html, true)?;
                        self.mode = Mode::BeforeHead;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if t.end && !matches!(t.name.as_str(), "head" | "body" | "html" | "br") =>
                    {
                        self.error();
                        return Ok(());
                    }
                    _ => {
                        self.insert_named("html")?;
                        self.mode = Mode::BeforeHead;
                    }
                },
                Mode::BeforeHead => match token {
                    Token::Comment(s) => return self.comment(s, self.current()),
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    Token::Tag(t) if !t.end && t.name == "head" => {
                        self.head = Some(self.insert_element(t, Namespace::Html, true)?);
                        self.mode = Mode::Head;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if t.end && !matches!(t.name.as_str(), "head" | "body" | "html" | "br") =>
                    {
                        self.error();
                        return Ok(());
                    }
                    _ => {
                        self.head = Some(self.insert_named("head")?);
                        self.mode = Mode::Head;
                    }
                },
                Mode::Head => {
                    if self.head_token(token)? {
                        return Ok(());
                    }
                    self.open.pop();
                    self.mode = Mode::AfterHead;
                }
                Mode::HeadNoscript => match token {
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if t.end && t.name == "noscript" => {
                        self.open.pop();
                        self.mode = Mode::Head;
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    Token::Comment(_) => {
                        self.head_token(token)?;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if !t.end
                            && matches!(
                                t.name.as_str(),
                                "basefont" | "bgsound" | "link" | "meta" | "noframes" | "style"
                            ) =>
                    {
                        self.head_token(token)?;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if (t.end && t.name != "br")
                            || (!t.end && matches!(t.name.as_str(), "head" | "noscript")) =>
                    {
                        self.error();
                        return Ok(());
                    }
                    _ => {
                        self.error();
                        self.open.pop();
                        self.mode = Mode::Head;
                    }
                },
                Mode::AfterHead => match token {
                    Token::Comment(s) => return self.comment(s, self.current()),
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    Token::Tag(t) if !t.end && t.name == "body" => {
                        self.insert_element(t, Namespace::Html, true)?;
                        self.mode = Mode::Body;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if !t.end
                            && matches!(
                                t.name.as_str(),
                                "base"
                                    | "basefont"
                                    | "bgsound"
                                    | "link"
                                    | "meta"
                                    | "noframes"
                                    | "script"
                                    | "style"
                                    | "template"
                                    | "title"
                            ) =>
                    {
                        let head = self.head.ok_or(ParseError::State)?;
                        self.push_open(head)?;
                        self.head_token(token)?;
                        if let Some(i) = self.open_position(head)? {
                            self.open.remove(i);
                        }
                        return Ok(());
                    }
                    Token::Tag(t) if t.end && t.name == "template" => {
                        self.head_token(token)?;
                        return Ok(());
                    }
                    Token::Tag(t)
                        if (!t.end && t.name == "head")
                            || (t.end && !matches!(t.name.as_str(), "body" | "html" | "br")) =>
                    {
                        self.error();
                        return Ok(());
                    }
                    _ => {
                        self.insert_named("body")?;
                        self.mode = Mode::Body;
                    }
                },
                Mode::Text => {
                    if let Token::Tag(t) = token {
                        if t.end {
                            self.open.pop();
                            self.mode = self.original;
                            return Ok(());
                        }
                    }
                    return Err(ParseError::State);
                }
                Mode::Body => return self.body_token(token),
                Mode::Table
                | Mode::Caption
                | Mode::ColumnGroup
                | Mode::TableBody
                | Mode::Row
                | Mode::Cell => return self.table_token(token),
                Mode::TableText => return Err(ParseError::State),
                Mode::Select | Mode::SelectTable => return self.select_token(token),
                Mode::Template => match token {
                    Token::Comment(_) | Token::Doctype(_) => return self.body_token(token),
                    Token::Tag(t)
                        if (!t.end
                            && matches!(
                                t.name.as_str(),
                                "base"
                                    | "basefont"
                                    | "bgsound"
                                    | "link"
                                    | "meta"
                                    | "noframes"
                                    | "script"
                                    | "style"
                                    | "template"
                                    | "title"
                            ))
                            || (t.end && t.name == "template") =>
                    {
                        self.head_token(token)?;
                        return Ok(());
                    }
                    Token::Tag(t) if t.end => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) => {
                        let mode = match t.name.as_str() {
                            "caption" | "colgroup" | "tbody" | "tfoot" | "thead" => Mode::Table,
                            "col" => Mode::ColumnGroup,
                            "tr" => Mode::TableBody,
                            "td" | "th" => Mode::Row,
                            _ => Mode::Body,
                        };
                        self.template_mode(mode)?;
                    }
                    Token::Text(_) => return Err(ParseError::State),
                },
                Mode::AfterBody => match token {
                    Token::Comment(s) => {
                        let html = self.open.first().copied().ok_or(ParseError::State)?;
                        return self.comment(s, html);
                    }
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    Token::Tag(t) if t.end && t.name == "html" => {
                        self.mode = Mode::AfterAfterBody;
                        return Ok(());
                    }
                    _ => {
                        self.error();
                        self.mode = Mode::Body;
                    }
                },
                Mode::AfterAfterBody => match token {
                    Token::Comment(s) => return self.comment(s, self.doc.root),
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    _ => {
                        self.error();
                        self.mode = Mode::Body;
                    }
                },
            }
        }
    }
    fn template_mode(&mut self, mode: Mode) -> Result<(), ParseError> {
        *self.templates.last_mut().ok_or(ParseError::State)? = mode;
        self.mode = mode;
        Ok(())
    }
    pub(super) fn head_token(&mut self, token: &Token) -> Result<bool, ParseError> {
        match token {
            Token::Comment(s) => self.comment(s, self.current())?,
            Token::Doctype(_) => self.error(),
            Token::Tag(t) => match (t.end, t.name.as_str()) {
                (false, "html") => self.body(t)?,
                (false, "base" | "basefont" | "bgsound" | "link" | "meta") => {
                    self.insert_element(t, Namespace::Html, false)?;
                }
                (false, "title") => self.start_text(t, TextMode::Rcdata)?,
                (false, "style" | "noframes") => self.start_text(t, TextMode::Rawtext)?,
                (false, "script") => self.start_text(t, TextMode::Script)?,
                (false, "noscript") => {
                    self.insert_element(t, Namespace::Html, true)?;
                    self.mode = Mode::HeadNoscript;
                }
                (false, "template") => {
                    let id = self.insert_element(t, Namespace::Html, true)?;
                    let fragment = self.allocate(NodeKind::DocumentFragment)?;
                    if let NodeKind::Element(e) = &mut self.node_mut(id)?.kind {
                        e.template_contents = Some(fragment);
                    }
                    self.active.try_push(None)?;
                    self.templates.try_push(Mode::Template)?;
                    self.mode = Mode::Template;
                }
                (true, "template") => self.close_template()?,
                (true, "head") => {
                    self.open.pop();
                    self.mode = Mode::AfterHead;
                }
                (false, "head") => self.error(),
                (true, name) if !matches!(name, "body" | "html" | "br") => self.error(),
                _ => return Ok(false),
            },
            Token::Text(_) => return Err(ParseError::State),
        }
        Ok(true)
    }
    pub(super) fn body_token(&mut self, token: &Token) -> Result<(), ParseError> {
        match token {
            Token::Comment(s) => self.comment(s, self.current()),
            Token::Doctype(_) => {
                self.error();
                Ok(())
            }
            Token::Tag(t) => self.body(t),
            Token::Text(_) => Err(ParseError::State),
        }
    }
    pub(super) fn body(&mut self, t: &Tag) -> Result<(), ParseError> {
        let name = t.name.as_str();
        if t.end {
            return self.body_end(name);
        }
        match name {
            "html" => {
                if self.templates.is_empty() {
                    if let Some(id) = self.open.first().copied() {
                        self.merge_attributes(id, t)?;
                    }
                }
            }
            "base" | "basefont" | "bgsound" | "link" | "meta" | "noframes" | "script" | "style"
            | "template" | "title" => {
                let copy = self.copy_tag(t)?;
                self.head_token(&Token::Tag(copy))?;
            }
            "body" => {
                if self.templates.is_empty() {
                    if let Some(i) = self.find_open("body")? {
                        let id = *self.open.get(i).ok_or(ParseError::State)?;
                        self.merge_attributes(id, t)?;
                    }
                }
            }
            "address" | "article" | "aside" | "blockquote" | "center" | "details" | "dialog"
            | "dir" | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer" | "header"
            | "hgroup" | "main" | "menu" | "nav" | "ol" | "p" | "search" | "section"
            | "summary" | "ul" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, true)?;
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                if self.current_element().is_some_and(|e| heading(&e.name)) {
                    self.open.pop();
                }
                self.insert_element(t, Namespace::Html, true)?;
            }
            "pre" | "listing" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, true)?;
                self.ignore_lf = true;
            }
            "form" => {
                if self.form.is_some() && self.templates.is_empty() {
                    self.error();
                    return Ok(());
                }
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                let id = self.insert_element(t, Namespace::Html, true)?;
                if self.templates.is_empty() {
                    self.form = Some(id);
                }
            }
            "li" | "dd" | "dt" => {
                for i in (0..self.open.len()).rev() {
                    self.step()?;
                    let id = *self.open.get(i).ok_or(ParseError::State)?;
                    if self.is(id, name)
                        || (name != "li" && (self.is(id, "dd") || self.is(id, "dt")))
                    {
                        let except = if self.is(id, "dd") {
                            "dd"
                        } else if self.is(id, "dt") {
                            "dt"
                        } else {
                            "li"
                        };
                        self.implied(except, false)?;
                        self.open.truncate(i);
                        break;
                    }
                    if self
                        .node(id)?
                        .element()
                        .is_some_and(super::foreign::special)
                        && !["address", "div", "p"].iter().any(|n| self.is(id, n))
                    {
                        break;
                    }
                }
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, true)?;
            }
            "plaintext" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, true)?;
                self.tok.set_text_mode(TextMode::Plaintext);
            }
            "button" => {
                if self.scoped("button", "")? {
                    self.implied("", false)?;
                    self.pop_through("button")?;
                }
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, true)?;
            }
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small"
            | "strike" | "strong" | "tt" | "u" => {
                if name == "a" {
                    if let Some((_, old)) = self.active_named("a")? {
                        self.adoption("a")?;
                        if let Some(i) = self.active_position(old)? {
                            self.active.remove(i);
                        }
                        if let Some(i) = self.open_position(old)? {
                            self.open.remove(i);
                        }
                    }
                }
                self.reconstruct()?;
                if name == "nobr" && self.scoped("nobr", "")? {
                    self.adoption("nobr")?;
                    self.reconstruct()?;
                }
                let id = self.insert_element(t, Namespace::Html, true)?;
                self.add_active(id)?;
            }
            "applet" | "marquee" | "object" => {
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, true)?;
                self.active.try_push(None)?;
            }
            "table" => {
                if self.doc.quirks != QuirksMode::Quirks && self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, true)?;
                self.mode = Mode::Table;
            }
            "area" | "br" | "embed" | "img" | "input" | "keygen" | "wbr" => {
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, false)?;
            }
            "param" | "source" | "track" => {
                self.insert_element(t, Namespace::Html, false)?;
            }
            "hr" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.insert_element(t, Namespace::Html, false)?;
            }
            "image" => {
                let mut replacement = self.copy_tag(t)?;
                replacement.name = self.owned("img")?;
                self.body(&replacement)?;
            }
            "textarea" => {
                self.start_text(t, TextMode::Rcdata)?;
                self.ignore_lf = true;
            }
            "xmp" => {
                if self.scoped("p", "button")? {
                    self.close_p()?;
                }
                self.reconstruct()?;
                self.start_text(t, TextMode::Rawtext)?;
            }
            "iframe" | "noembed" => self.start_text(t, TextMode::Rawtext)?,
            "select" => {
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, true)?;
                self.mode = if matches!(
                    self.mode,
                    Mode::Table | Mode::Caption | Mode::TableBody | Mode::Row | Mode::Cell
                ) {
                    Mode::SelectTable
                } else {
                    Mode::Select
                };
            }
            "optgroup" | "option" => {
                if self.current_is("option") {
                    self.open.pop();
                }
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, true)?;
            }
            "rb" | "rtc" => {
                if self.scoped("ruby", "")? {
                    self.implied("", false)?;
                }
                self.insert_element(t, Namespace::Html, true)?;
            }
            "rp" | "rt" => {
                if self.scoped("ruby", "")? {
                    self.implied("rtc", false)?;
                }
                self.insert_element(t, Namespace::Html, true)?;
            }
            "math" | "svg" => {
                self.reconstruct()?;
                self.insert_element(
                    t,
                    if name == "math" {
                        Namespace::MathMl
                    } else {
                        Namespace::Svg
                    },
                    !t.self_closing,
                )?;
            }
            "caption" | "col" | "colgroup" | "frame" | "head" | "tbody" | "td" | "tfoot" | "th"
            | "thead" | "tr" | "frameset" => self.error(),
            _ => {
                self.reconstruct()?;
                self.insert_element(t, Namespace::Html, true)?;
            }
        }
        Ok(())
    }
    fn body_end(&mut self, name: &str) -> Result<(), ParseError> {
        match name {
            "template" => self.close_template()?,
            "body" | "html" => {
                if self.scoped("body", "")? {
                    self.mode = if name == "html" {
                        Mode::AfterAfterBody
                    } else {
                        Mode::AfterBody
                    };
                } else {
                    self.error();
                }
            }
            "p" => self.close_p()?,
            "li" | "dd" | "dt" => {
                if self.scoped(name, if name == "li" { "list" } else { "" })? {
                    self.implied(name, false)?;
                    self.pop_through(name)?;
                } else {
                    self.error();
                }
            }
            "address" | "article" | "aside" | "blockquote" | "button" | "center" | "details"
            | "dialog" | "dir" | "div" | "dl" | "fieldset" | "figcaption" | "figure" | "footer"
            | "header" | "hgroup" | "listing" | "main" | "menu" | "nav" | "ol" | "pre"
            | "search" | "section" | "summary" | "ul" => {
                if self.scoped(name, "")? {
                    self.implied("", false)?;
                    self.pop_through(name)?;
                } else {
                    self.error();
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let mut found = false;
                for h in ["h1", "h2", "h3", "h4", "h5", "h6"] {
                    if self.scoped(h, "")? {
                        found = true;
                        break;
                    }
                }
                if found {
                    self.implied("", false)?;
                    while let Some(id) = self.open.pop() {
                        self.step()?;
                        if self.node(id)?.element().is_some_and(|e| heading(&e.name)) {
                            break;
                        }
                    }
                } else {
                    self.error();
                }
            }
            "form" if self.templates.is_empty() => {
                let form = self.form.take();
                if let Some(id) = form {
                    if self.scoped("form", "")? {
                        self.implied("", false)?;
                        if let Some(i) = self.open_position(id)? {
                            self.open.remove(i);
                        }
                    }
                }
            }
            "form" => {
                if self.scoped("form", "")? {
                    self.implied("", false)?;
                    self.pop_through("form")?;
                }
            }
            "a" | "b" | "big" | "code" | "em" | "font" | "i" | "nobr" | "s" | "small"
            | "strike" | "strong" | "tt" | "u" => self.adoption(name)?,
            "applet" | "marquee" | "object" => {
                if self.scoped(name, "")? {
                    self.implied("", false)?;
                    self.pop_through(name)?;
                    self.clear_active()?;
                } else {
                    self.error();
                }
            }
            "br" => {
                self.error();
                let t = self.synthetic("br", false)?;
                self.body(&t)?;
            }
            _ => self.any_end(name)?,
        }
        Ok(())
    }
    pub(super) fn copy_tag(&mut self, t: &Tag) -> Result<Tag, ParseError> {
        let name = self.owned(&t.name)?;
        let mut attributes =
            td_alloc::BudgetVec::with_capacity(&self.tok.budget.memory, t.attributes.len())?;
        for a in &t.attributes {
            self.tok.budget.attribute()?;
            attributes.try_push(html5::Attribute {
                name: self.owned(&a.name)?,
                value: self.owned(&a.value)?,
            })?;
        }
        Ok(Tag {
            name,
            attributes,
            end: t.end,
            self_closing: t.self_closing,
        })
    }
}
pub(super) fn ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}
fn heading(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}
fn quirks(d: &html5::Doctype) -> QuirksMode {
    if d.force_quirks || d.name != "html" {
        return QuirksMode::Quirks;
    }
    let public = d.public_id.as_deref().unwrap_or_default();
    let system = d.system_id.as_deref().unwrap_or_default();
    if QUIRKS_EXACT.iter().any(|p| public.eq_ignore_ascii_case(p))
        || QUIRKS_PREFIX.iter().any(|p| starts(public, p))
        || system.eq_ignore_ascii_case("http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd")
    {
        return QuirksMode::Quirks;
    }
    if starts(public, "-//w3c//dtd html 4.01 frameset//")
        || starts(public, "-//w3c//dtd html 4.01 transitional//")
    {
        return if d.system_id.is_none() {
            QuirksMode::Quirks
        } else {
            QuirksMode::LimitedQuirks
        };
    }
    if starts(public, "-//w3c//dtd xhtml 1.0 frameset//")
        || starts(public, "-//w3c//dtd xhtml 1.0 transitional//")
    {
        return QuirksMode::LimitedQuirks;
    }
    QuirksMode::NoQuirks
}

fn starts(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(prefix))
}
const QUIRKS_EXACT: &[&str] = &[
    "-//w3o//dtd w3 html strict 3.0//en//",
    "-/w3c/dtd html 4.0 transitional/en",
    "html",
];
const QUIRKS_PREFIX: &[&str] = &[
    "+//silmaril//dtd html pro v0r11 19970101//",
    "-//as//dtd html 3.0 aswedit + extensions//",
    "-//advasoft ltd//dtd html 3.0 aswedit + extensions//",
    "-//ietf//dtd html 2.0 level 1//",
    "-//ietf//dtd html 2.0 level 2//",
    "-//ietf//dtd html 2.0 strict level 1//",
    "-//ietf//dtd html 2.0 strict level 2//",
    "-//ietf//dtd html 2.0 strict//",
    "-//ietf//dtd html 2.0//",
    "-//ietf//dtd html 2.1e//",
    "-//ietf//dtd html 3.0//",
    "-//ietf//dtd html 3.2 final//",
    "-//ietf//dtd html 3.2//",
    "-//ietf//dtd html 3//",
    "-//ietf//dtd html level 0//",
    "-//ietf//dtd html level 1//",
    "-//ietf//dtd html level 2//",
    "-//ietf//dtd html level 3//",
    "-//ietf//dtd html strict level 0//",
    "-//ietf//dtd html strict level 1//",
    "-//ietf//dtd html strict level 2//",
    "-//ietf//dtd html strict level 3//",
    "-//ietf//dtd html strict//",
    "-//ietf//dtd html//",
    "-//metrius//dtd metrius presentational//",
    "-//microsoft//dtd internet explorer 2.0 html strict//",
    "-//microsoft//dtd internet explorer 2.0 html//",
    "-//microsoft//dtd internet explorer 2.0 tables//",
    "-//microsoft//dtd internet explorer 3.0 html strict//",
    "-//microsoft//dtd internet explorer 3.0 html//",
    "-//microsoft//dtd internet explorer 3.0 tables//",
    "-//netscape comm. corp.//dtd html//",
    "-//netscape comm. corp.//dtd strict html//",
    "-//o'reilly and associates//dtd html 2.0//",
    "-//o'reilly and associates//dtd html extended 1.0//",
    "-//o'reilly and associates//dtd html extended relaxed 1.0//",
    "-//sq//dtd html 2.0 hotmetal + extensions//",
    "-//softquad software//dtd hotmetal pro 6.0::19990601::extensions to html 4.0//",
    "-//softquad//dtd hotmetal pro 4.0::19971010::extensions to html 4.0//",
    "-//spyglass//dtd html 2.0 extended//",
    "-//sun microsystems corp.//dtd hotjava html//",
    "-//sun microsystems corp.//dtd hotjava strict html//",
    "-//w3c//dtd html 3 1995-03-24//",
    "-//w3c//dtd html 3.2 draft//",
    "-//w3c//dtd html 3.2 final//",
    "-//w3c//dtd html 3.2//",
    "-//w3c//dtd html 3.2s draft//",
    "-//w3c//dtd html 4.0 frameset//",
    "-//w3c//dtd html 4.0 transitional//",
    "-//w3c//dtd html experimental 19960712//",
    "-//w3c//dtd html experimental 970421//",
    "-//w3c//dtd w3 html//",
    "-//w3o//dtd w3 html 3.0//",
    "-//webtechs//dtd mozilla html 2.0//",
    "-//webtechs//dtd mozilla html//",
];
