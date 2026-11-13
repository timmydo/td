use super::builder::{Builder, Mode};
use super::*;
impl Builder<'_> {
    pub(super) fn table_token(&mut self, token: &Token) -> Result<(), ParseError> {
        loop {
            self.step()?;
            let tag = if let Token::Tag(t) = token {
                Some(t)
            } else {
                None
            };
            match self.mode {
                Mode::Caption => {
                    if tag.is_some_and(|t| {
                        (t.end && t.name == "caption")
                            || (!t.end
                                && matches!(
                                    t.name.as_str(),
                                    "caption"
                                        | "col"
                                        | "colgroup"
                                        | "tbody"
                                        | "td"
                                        | "tfoot"
                                        | "th"
                                        | "thead"
                                        | "tr"
                                ))
                            || (t.end && t.name == "table")
                    }) {
                        if !self.scoped("caption", "table")? {
                            self.error();
                            return Ok(());
                        }
                        self.implied("", false)?;
                        self.pop_through("caption")?;
                        self.clear_active()?;
                        self.mode = Mode::Table;
                        if tag.is_some_and(|t| t.end && t.name == "caption") {
                            return Ok(());
                        }
                        continue;
                    }
                    if tag.is_some_and(|t| {
                        t.end
                            && matches!(
                                t.name.as_str(),
                                "body"
                                    | "col"
                                    | "colgroup"
                                    | "html"
                                    | "tbody"
                                    | "td"
                                    | "tfoot"
                                    | "th"
                                    | "thead"
                                    | "tr"
                            )
                    }) {
                        self.error();
                        return Ok(());
                    }
                    return self.body_token(token);
                }
                Mode::ColumnGroup => match token {
                    Token::Comment(s) => return self.comment(s, self.current()),
                    Token::Doctype(_) => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if !t.end && t.name == "html" => return self.body(t),
                    Token::Tag(t) if !t.end && t.name == "col" => {
                        self.insert_element(t, Namespace::Html, false)?;
                        return Ok(());
                    }
                    Token::Tag(t) if t.end && t.name == "colgroup" => {
                        if self.current_is("colgroup") {
                            self.open.pop();
                            self.mode = Mode::Table;
                        } else {
                            self.error();
                        }
                        return Ok(());
                    }
                    Token::Tag(t) if t.end && t.name == "col" => {
                        self.error();
                        return Ok(());
                    }
                    Token::Tag(t) if t.name == "template" => {
                        self.head_token(token)?;
                        return Ok(());
                    }
                    _ => {
                        if !self.current_is("colgroup") {
                            self.error();
                            return Ok(());
                        }
                        self.open.pop();
                        self.mode = Mode::Table;
                    }
                },
                Mode::TableBody => {
                    if let Some(t) = tag {
                        match (t.end, t.name.as_str()) {
                            (false, "tr") => {
                                self.clear_to(&["tbody", "tfoot", "thead", "template", "html"])?;
                                self.insert_element(t, Namespace::Html, true)?;
                                self.mode = Mode::Row;
                                return Ok(());
                            }
                            (false, "td" | "th") => {
                                self.error();
                                self.clear_to(&["tbody", "tfoot", "thead", "template", "html"])?;
                                self.insert_named("tr")?;
                                self.mode = Mode::Row;
                                continue;
                            }
                            (true, "tbody" | "tfoot" | "thead") => {
                                if self.scoped(&t.name, "table")? {
                                    self.clear_to(&[
                                        "tbody", "tfoot", "thead", "template", "html",
                                    ])?;
                                    self.open.pop();
                                    self.mode = Mode::Table;
                                } else {
                                    self.error();
                                }
                                return Ok(());
                            }
                            (
                                false,
                                "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead",
                            )
                            | (true, "table") => {
                                if !self.has_table_body()? {
                                    self.error();
                                    return Ok(());
                                }
                                self.clear_to(&["tbody", "tfoot", "thead", "template", "html"])?;
                                self.open.pop();
                                self.mode = Mode::Table;
                                continue;
                            }
                            (
                                true,
                                "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th"
                                | "tr",
                            ) => {
                                self.error();
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    return self.in_table(token);
                }
                Mode::Row => {
                    if let Some(t) = tag {
                        match (t.end, t.name.as_str()) {
                            (false, "td" | "th") => {
                                self.clear_to(&["tr", "template", "html"])?;
                                self.insert_element(t, Namespace::Html, true)?;
                                self.mode = Mode::Cell;
                                self.active.try_push(None)?;
                                return Ok(());
                            }
                            (true, "tr") => {
                                self.close_row()?;
                                return Ok(());
                            }
                            (
                                false,
                                "caption" | "col" | "colgroup" | "tbody" | "tfoot" | "thead" | "tr",
                            )
                            | (true, "table") => {
                                if !self.close_row()? {
                                    return Ok(());
                                }
                                continue;
                            }
                            (true, "tbody" | "tfoot" | "thead") => {
                                if !self.scoped(&t.name, "table")? {
                                    self.error();
                                    return Ok(());
                                }
                                if !self.close_row()? {
                                    return Ok(());
                                }
                                continue;
                            }
                            (
                                true,
                                "body" | "caption" | "col" | "colgroup" | "html" | "td" | "th",
                            ) => {
                                self.error();
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    return self.in_table(token);
                }
                Mode::Cell => {
                    if let Some(t) = tag {
                        match (t.end, t.name.as_str()) {
                            (true, "td" | "th") => {
                                if self.scoped(&t.name, "table")? {
                                    self.implied("", false)?;
                                    self.pop_through(&t.name)?;
                                    self.clear_active()?;
                                    self.mode = Mode::Row;
                                } else {
                                    self.error();
                                }
                                return Ok(());
                            }
                            (
                                false,
                                "caption" | "col" | "colgroup" | "tbody" | "td" | "tfoot" | "th"
                                | "thead" | "tr",
                            ) => {
                                if !self.close_cell()? {
                                    return Ok(());
                                }
                                continue;
                            }
                            (true, "table" | "tbody" | "tfoot" | "thead" | "tr") => {
                                if !self.scoped(&t.name, "table")? {
                                    self.error();
                                    return Ok(());
                                }
                                if !self.close_cell()? {
                                    return Ok(());
                                }
                                continue;
                            }
                            (true, "body" | "caption" | "col" | "colgroup" | "html") => {
                                self.error();
                                return Ok(());
                            }
                            _ => {}
                        }
                    }
                    return self.body_token(token);
                }
                Mode::Table => return self.in_table(token),
                _ => return self.process(token),
            }
        }
    }
    fn has_table_body(&mut self) -> Result<bool, ParseError> {
        for name in ["tbody", "thead", "tfoot"] {
            if self.scoped(name, "table")? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn close_row(&mut self) -> Result<bool, ParseError> {
        if !self.scoped("tr", "table")? {
            self.error();
            return Ok(false);
        }
        self.clear_to(&["tr", "template", "html"])?;
        self.open.pop();
        self.mode = Mode::TableBody;
        Ok(true)
    }
    fn close_cell(&mut self) -> Result<bool, ParseError> {
        let name = if self.scoped("td", "table")? {
            "td"
        } else if self.scoped("th", "table")? {
            "th"
        } else {
            self.error();
            return Ok(false);
        };
        self.implied("", false)?;
        self.pop_through(name)?;
        self.clear_active()?;
        self.mode = Mode::Row;
        Ok(true)
    }
    fn in_table(&mut self, token: &Token) -> Result<(), ParseError> {
        if let Token::Comment(s) = token {
            return self.comment(s, self.current());
        }
        if let Token::Doctype(_) = token {
            self.error();
            return Ok(());
        }
        if let Token::Tag(t) = token {
            match (t.end, t.name.as_str()) {
                (false, "caption") => {
                    self.clear_to(&["table", "template", "html"])?;
                    self.active.try_push(None)?;
                    self.insert_element(t, Namespace::Html, true)?;
                    self.mode = Mode::Caption;
                    return Ok(());
                }
                (false, "colgroup") => {
                    self.clear_to(&["table", "template", "html"])?;
                    self.insert_element(t, Namespace::Html, true)?;
                    self.mode = Mode::ColumnGroup;
                    return Ok(());
                }
                (false, "col") => {
                    self.clear_to(&["table", "template", "html"])?;
                    self.insert_named("colgroup")?;
                    self.mode = Mode::ColumnGroup;
                    return self.table_token(token);
                }
                (false, "tbody" | "tfoot" | "thead") => {
                    self.clear_to(&["table", "template", "html"])?;
                    self.insert_element(t, Namespace::Html, true)?;
                    self.mode = Mode::TableBody;
                    return Ok(());
                }
                (false, "td" | "th" | "tr") => {
                    self.clear_to(&["table", "template", "html"])?;
                    self.insert_named("tbody")?;
                    self.mode = Mode::TableBody;
                    return self.table_token(token);
                }
                (false, "table") => {
                    if self.scoped("table", "table")? {
                        self.pop_through("table")?;
                        self.reset_mode()?;
                        return self.process(token);
                    }
                    self.error();
                    return Ok(());
                }
                (true, "table") => {
                    if self.scoped("table", "table")? {
                        self.pop_through("table")?;
                        self.reset_mode()?;
                    } else {
                        self.error();
                    }
                    return Ok(());
                }
                (
                    true,
                    "body" | "caption" | "col" | "colgroup" | "html" | "tbody" | "td" | "tfoot"
                    | "th" | "thead" | "tr",
                ) => {
                    self.error();
                    return Ok(());
                }
                (false, "style" | "script" | "template") | (true, "template") => {
                    self.head_token(token)?;
                    return Ok(());
                }
                (false, "input")
                    if t.attributes
                        .iter()
                        .any(|a| a.name == "type" && a.value.eq_ignore_ascii_case("hidden")) =>
                {
                    self.insert_element(t, Namespace::Html, false)?;
                    return Ok(());
                }
                (false, "form") => {
                    if self.form.is_none() && self.templates.is_empty() {
                        self.form = Some(self.insert_element(t, Namespace::Html, false)?);
                    } else {
                        self.error();
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        self.error();
        self.foster = true;
        let result = self.body_token(token);
        self.foster = false;
        result
    }
    pub(super) fn select_token(&mut self, token: &Token) -> Result<(), ParseError> {
        if self.mode == Mode::SelectTable {
            if let Token::Tag(t) = token {
                if matches!(
                    t.name.as_str(),
                    "caption" | "table" | "tbody" | "tfoot" | "thead" | "tr" | "td" | "th"
                ) {
                    if t.end && !self.scoped(&t.name, "table")? {
                        self.error();
                        return Ok(());
                    }
                    self.pop_through("select")?;
                    self.reset_mode()?;
                    return self.process(token);
                }
            }
        }
        match token {
            Token::Comment(s) => self.comment(s, self.current())?,
            Token::Doctype(_) => self.error(),
            Token::Tag(t) => match (t.end, t.name.as_str()) {
                (false, "html") => self.body(t)?,
                (false, "option") => {
                    if self.current_is("option") {
                        self.open.pop();
                    }
                    self.insert_element(t, Namespace::Html, true)?;
                }
                (false, "optgroup") => {
                    if self.current_is("option") {
                        self.open.pop();
                    }
                    if self.current_is("optgroup") {
                        self.open.pop();
                    }
                    self.insert_element(t, Namespace::Html, true)?;
                }
                (false, "hr") => {
                    if self.current_is("option") {
                        self.open.pop();
                    }
                    if self.current_is("optgroup") {
                        self.open.pop();
                    }
                    self.insert_element(t, Namespace::Html, false)?;
                }
                (true, "optgroup") => {
                    if self.current_is("option")
                        && self
                            .open
                            .iter()
                            .rev()
                            .nth(1)
                            .is_some_and(|id| self.is(*id, "optgroup"))
                    {
                        self.open.pop();
                    }
                    if self.current_is("optgroup") {
                        self.open.pop();
                    } else {
                        self.error();
                    }
                }
                (true, "option") => {
                    if self.current_is("option") {
                        self.open.pop();
                    } else {
                        self.error();
                    }
                }
                (_, "select") => {
                    if self.scoped("select", "select")? {
                        self.pop_through("select")?;
                        self.reset_mode()?;
                    } else {
                        self.error();
                    }
                }
                (false, "input" | "keygen" | "textarea") => {
                    if self.scoped("select", "select")? {
                        self.pop_through("select")?;
                        self.reset_mode()?;
                        self.process(token)?;
                    }
                }
                (false, "script" | "template") | (true, "template") => {
                    self.head_token(token)?;
                }
                _ => self.error(),
            },
            Token::Text(_) => return Err(ParseError::State),
        }
        Ok(())
    }
}
