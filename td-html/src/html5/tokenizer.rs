use super::{entities::ENTITIES, Budget, ParseError, ParseLimits};
use td_alloc::{BudgetString, BudgetVec};

#[derive(Debug, PartialEq, Eq)]
pub struct Attribute {
    pub name: BudgetString,
    pub value: BudgetString,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Tag {
    pub name: BudgetString,
    pub attributes: BudgetVec<Attribute>,
    pub self_closing: bool,
    pub end: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub struct Doctype {
    pub name: BudgetString,
    pub public_id: Option<BudgetString>,
    pub system_id: Option<BudgetString>,
    pub force_quirks: bool,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Token {
    Tag(Tag),
    Text(BudgetString),
    Comment(BudgetString),
    Doctype(Doctype),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextMode {
    Data,
    Rcdata,
    Rawtext,
    Script,
    Plaintext,
}

/// The builder selects text and CDATA context after accepting a start tag.
pub struct Tokenizer<'a> {
    input: &'a str,
    position: usize,
    mode: TextMode,
    last_start: BudgetString,
    script_escape: u8,
    script_dashes: u8,
    foreign: bool,
    finished: bool,
    failure: Option<ParseError>,
    errors: usize,
    pub(crate) budget: Budget,
    spare_tag: Option<Tag>,
    spare_attributes: [Option<Attribute>; 8],
    #[cfg(test)]
    pub(crate) scalar_runs: bool,
}
impl<'a> Tokenizer<'a> {
    pub fn new(input: &'a str, limits: ParseLimits) -> Result<Self, ParseError> {
        if input.len() > limits.clamped().input {
            return Err(ParseError::Input);
        }
        let memory = td_alloc::Budget::new(
            crate::html5::MAX_TREE_BYTES + td_alloc::Budget::metadata_bytes(),
        )?;
        Self::with_budget(input, limits, &memory)
    }
    pub fn with_budget(
        input: &'a str,
        limits: ParseLimits,
        memory: &td_alloc::Budget,
    ) -> Result<Self, ParseError> {
        let budget = Budget::new(limits, memory)?;
        if input.len() > budget.limits.input {
            return Err(ParseError::Input);
        }
        Ok(Self {
            input,
            position: usize::from(input.starts_with('\u{feff}')) * 3,
            mode: TextMode::Data,
            last_start: BudgetString::new(memory)?,
            script_escape: 0,
            script_dashes: 0,
            foreign: false,
            finished: false,
            failure: None,
            errors: 0,
            budget,
            spare_tag: None,
            spare_attributes: std::array::from_fn(|_| None),
            #[cfg(test)]
            scalar_runs: false,
        })
    }
    // The tree builder consumes each token before asking for another. Reuse
    // its small scratch buffers; external tokenizer clients retain tokens as
    // before. All cached capacities keep their original budget admission.
    pub(crate) fn recycle(&mut self, token: Token) {
        if let Token::Tag(mut tag) = token {
            tag.name.clear();
            while let Some(mut attribute) = tag.attributes.pop() {
                if attribute.name.capacity() <= 1024 && attribute.value.capacity() <= 1024 {
                    if let Some(slot) = self.spare_attributes.iter_mut().find(|slot| slot.is_none())
                    {
                        attribute.name.clear();
                        attribute.value.clear();
                        *slot = Some(attribute);
                    }
                }
            }
            self.spare_tag = Some(tag);
        }
    }
    pub fn set_text_mode(&mut self, mode: TextMode) {
        self.mode = mode;
        self.script_escape = 0;
        self.script_dashes = 0;
    }
    pub fn set_foreign(&mut self, foreign: bool) {
        self.foreign = foreign;
    }
    pub fn errors(&self) -> usize {
        self.errors
    }
    pub fn charged_bytes(&self) -> usize {
        self.budget.bytes
    }
    pub fn work(&self) -> usize {
        self.budget.work
    }
    pub fn next_token(&mut self) -> Result<Option<Token>, ParseError> {
        if let Some(e) = self.failure {
            return Err(e);
        }
        if self.finished {
            return Ok(None);
        }
        let result = self.next();
        match &result {
            Err(e) => self.failure = Some(*e),
            Ok(None) => self.finished = true,
            _ => {}
        }
        result
    }
    fn rest(&self) -> &'a str {
        self.input.get(self.position..).unwrap_or_default()
    }
    fn peek(&self) -> Option<char> {
        self.rest()
            .chars()
            .next()
            .map(|c| if c == '\r' { '\n' } else { c })
    }
    fn take(&mut self) -> Result<Option<char>, ParseError> {
        let Some(c) = self.rest().chars().next() else {
            return Ok(None);
        };
        self.budget.step()?;
        self.position += c.len_utf8();
        if noncharacter(c as u32) || (control(c as u32) && c != '\0' && !ws(c)) {
            self.error();
        }
        if c == '\r' {
            if self.rest().starts_with('\n') {
                self.budget.step()?;
                self.position += 1;
            }
            return Ok(Some('\n'));
        }
        Ok(Some(c))
    }
    fn consume(&mut self, n: usize) -> Result<(), ParseError> {
        for _ in 0..n {
            self.take()?;
        }
        Ok(())
    }
    fn error(&mut self) {
        self.errors = self.errors.saturating_add(1);
    }
    fn starts(&self, s: &str) -> bool {
        self.rest()
            .get(..s.len())
            .is_some_and(|v| v.eq_ignore_ascii_case(s))
    }
    fn append_char(&mut self, s: &mut BudgetString, c: char) -> Result<(), ParseError> {
        self.budget.append_char(s, c, usize::MAX, ParseError::Bytes)
    }
    fn name(&mut self, s: &mut BudgetString, c: char) -> Result<(), ParseError> {
        if c == '\0' {
            self.error();
        }
        self.budget.append_char(
            s,
            replace_null(c).to_ascii_lowercase(),
            self.budget.limits.name_bytes,
            ParseError::NameBytes,
        )
    }
    fn value(&mut self, s: &mut BudgetString, c: char) -> Result<(), ParseError> {
        if c == '\0' {
            self.error();
        }
        self.budget.append_char(
            s,
            replace_null(c),
            self.budget.limits.attribute_bytes,
            ParseError::AttributeBytes,
        )
    }
    fn raw_end(&self) -> bool {
        if self.last_start.is_empty() || !self.rest().starts_with("</") {
            return false;
        }
        let tail = self.rest().get(2..).unwrap_or_default();
        tail.get(..self.last_start.len())
            .is_some_and(|n| n.eq_ignore_ascii_case(&self.last_start))
            && tail
                .get(self.last_start.len()..)
                .and_then(|s| s.chars().next())
                .is_some_and(|c| ws(c) || c == '/' || c == '>')
    }
    fn script_word(&self, word: &str) -> bool {
        self.starts(word)
            && self
                .rest()
                .get(word.len()..)
                .and_then(|s| s.chars().next())
                .is_some_and(|c| ws(c) || c == '/' || c == '>')
    }
    // Batch only already-normalized ASCII. Every consumed byte still funds
    // one tokenizer transition and the same output charge as scalar parsing.
    fn ascii_run(
        &mut self,
        out: &mut BudgetString,
        name: bool,
        quote: Option<char>,
    ) -> Result<bool, ParseError> {
        #[cfg(test)]
        if self.scalar_runs {
            return Ok(false);
        }
        let count = self
            .rest()
            .bytes()
            .take_while(|&b| {
                if name {
                    b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || matches!(b, b'-' | b'_' | b':' | b'.')
                } else {
                    (b' '..=b'~').contains(&b)
                        && b != b'&'
                        && match quote {
                            Some(q) => b != q as u8,
                            None => !matches!(b, b' ' | b'>' | b'<' | b'=' | b'`' | b'\'' | b'"'),
                        }
                }
            })
            .count();
        if count < 2 {
            return Ok(false);
        }
        let (limit, error) = if name {
            (self.budget.limits.name_bytes, ParseError::NameBytes)
        } else {
            (
                self.budget.limits.attribute_bytes,
                ParseError::AttributeBytes,
            )
        };
        if out.len().saturating_add(count) > limit {
            return Err(error);
        }
        self.budget.steps(count)?;
        let text = self.rest().get(..count).ok_or(ParseError::State)?;
        self.budget.append_text(out, text)?;
        self.position += count;
        Ok(true)
    }
    fn next(&mut self) -> Result<Option<Token>, ParseError> {
        let mut text = BudgetString::new(&self.budget.memory)?;
        loop {
            let Some(c) = self.peek() else {
                if self.mode == TextMode::Script && self.script_escape != 0 {
                    self.error();
                    self.script_escape = 0;
                }
                return Ok((!text.is_empty()).then_some(Token::Text(text)));
            };
            if self.mode == TextMode::Plaintext {
                self.take()?;
                if c == '\0' {
                    self.error();
                }
                self.append_char(&mut text, replace_null(c))?;
                continue;
            }
            if self.mode != TextMode::Data {
                if self.mode == TextMode::Script {
                    let transition = if self.script_escape == 0 && self.rest().starts_with("<!--") {
                        Some((4, 1))
                    } else if self.script_escape != 0 && self.rest().starts_with("-->") {
                        Some((3, 0))
                    } else if self.script_escape == 1 && self.script_word("<script") {
                        Some((7, 2))
                    } else if self.script_escape == 2 && self.script_word("</script") {
                        Some((8, 1))
                    } else {
                        None
                    };
                    if let Some((n, state)) = transition {
                        for _ in 0..n {
                            if let Some(c) = self.take()? {
                                self.append_char(&mut text, c)?;
                            }
                        }
                        self.script_escape = state;
                        self.script_dashes = if n == 4 { 2 } else { 0 };
                        continue;
                    }
                }
                if self.script_escape != 2 && self.raw_end() {
                    if !text.is_empty() {
                        return Ok(Some(Token::Text(text)));
                    }
                    self.consume(2)?;
                    self.mode = TextMode::Data;
                    return self.tag(true);
                }
                self.take()?;
                if self.mode == TextMode::Script && self.script_escape != 0 {
                    if c == '-' {
                        self.script_dashes = (self.script_dashes + 1).min(2);
                    } else {
                        if c == '>' && self.script_dashes == 2 {
                            self.script_escape = 0;
                        }
                        self.script_dashes = 0;
                    }
                }
                if c == '&' && self.mode == TextMode::Rcdata {
                    self.reference(&mut text, false)?;
                } else {
                    if c == '\0' {
                        self.error();
                    }
                    self.append_char(&mut text, replace_null(c))?;
                }
                continue;
            }
            if c != '<' {
                self.take()?;
                if c == '&' {
                    self.reference(&mut text, false)?;
                } else {
                    if c == '\0' {
                        self.error();
                    }
                    self.append_char(&mut text, c)?;
                }
                continue;
            }
            if !text.is_empty() {
                return Ok(Some(Token::Text(text)));
            }
            self.take()?;
            match self.peek() {
                Some(c) if c.is_ascii_alphabetic() => return self.tag(false),
                Some('/') => {
                    self.take()?;
                    match self.peek() {
                        Some(c) if c.is_ascii_alphabetic() => return self.tag(true),
                        Some('>') => {
                            self.take()?;
                            self.error();
                        }
                        None => {
                            self.error();
                            self.budget.append_text(&mut text, "</")?;
                        }
                        _ => {
                            self.error();
                            return self.bogus_comment();
                        }
                    }
                }
                Some('!') => {
                    self.take()?;
                    if self.rest().starts_with("--") {
                        self.consume(2)?;
                        return self.comment();
                    }
                    if self.starts("DOCTYPE") {
                        self.consume(7)?;
                        return self.doctype();
                    }
                    if self.foreign && self.rest().starts_with("[CDATA[") {
                        self.consume(7)?;
                        return self.cdata();
                    }
                    self.error();
                    return self.bogus_comment();
                }
                Some('?') => {
                    self.error();
                    return self.bogus_comment();
                }
                _ => {
                    self.error();
                    self.append_char(&mut text, '<')?;
                }
            }
        }
    }
    fn reference(&mut self, out: &mut BudgetString, attribute: bool) -> Result<(), ParseError> {
        if self.peek() == Some('#') {
            self.take()?;
            let hex = matches!(self.peek(), Some('x' | 'X'));
            let marker = self.peek();
            if hex {
                self.take()?;
            }
            let mut value = 0u32;
            let mut any = false;
            while let Some(n) = self
                .peek()
                .and_then(|c| c.to_digit(if hex { 16 } else { 10 }))
            {
                self.take()?;
                any = true;
                value = value
                    .saturating_mul(if hex { 16 } else { 10 })
                    .saturating_add(n);
            }
            if !any {
                self.error();
                for c in "&#".chars() {
                    if attribute {
                        self.value(out, c)?;
                    } else {
                        self.append_char(out, c)?;
                    }
                }
                if hex {
                    if let Some(c) = marker {
                        if attribute {
                            self.value(out, c)?;
                        } else {
                            self.append_char(out, c)?;
                        }
                    }
                }
                return Ok(());
            }
            if self.peek() == Some(';') {
                self.take()?;
            } else {
                self.error();
            }
            if value == 0
                || value > 0x10ffff
                || (0xd800..=0xdfff).contains(&value)
                || noncharacter(value)
                || value == 13
                || (control(value) && !matches!(value, 9 | 10 | 12 | 32))
            {
                self.error();
            }
            let c = super::super::from_codepoint(value);
            if attribute {
                self.value(out, c)
            } else {
                self.append_char(out, c)
            }
        } else {
            let mut best = None;
            for (offset, c) in self.rest().chars().take(32).enumerate() {
                self.budget.step()?;
                if !c.is_ascii_alphanumeric() && c != ';' {
                    break;
                }
                let length = offset + 1;
                let key = self.rest().get(..length).ok_or(ParseError::State)?;
                if let Ok(i) = ENTITIES.binary_search_by_key(&key, |entry| entry.0) {
                    best = ENTITIES.get(i).copied();
                }
                if c == ';' {
                    break;
                }
            }
            if let Some((name, value)) = best {
                let terminated = name.ends_with(';');
                let next = self.rest().get(name.len()..).and_then(|s| s.chars().next());
                if !terminated
                    && attribute
                    && next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '=')
                {
                    return self.value(out, '&');
                }
                self.consume(name.len())?;
                if !terminated {
                    self.error();
                }
                for c in value.chars() {
                    if attribute {
                        self.value(out, c)?;
                    } else {
                        if attribute {
                            self.value(out, c)?;
                        } else {
                            self.append_char(out, c)?;
                        }
                    }
                }
                Ok(())
            } else {
                if attribute {
                    self.value(out, '&')?;
                } else {
                    self.append_char(out, '&')?;
                }
                let ambiguous = self.peek().is_some_and(|c| c.is_ascii_alphanumeric());
                while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
                    if let Some(c) = self.take()? {
                        if attribute {
                            self.value(out, c)?;
                        } else {
                            self.append_char(out, c)?;
                        }
                    }
                }
                if ambiguous && self.peek() == Some(';') {
                    self.error();
                    self.take()?;
                    if attribute {
                        self.value(out, ';')?;
                    } else {
                        self.append_char(out, ';')?;
                    }
                }
                Ok(())
            }
        }
    }
    fn tag(&mut self, end: bool) -> Result<Option<Token>, ParseError> {
        let mut tag = match self.spare_tag.take() {
            Some(tag) => tag,
            None => Tag {
                name: BudgetString::new(&self.budget.memory)?,
                attributes: BudgetVec::new(&self.budget.memory)?,
                self_closing: false,
                end,
            },
        };
        tag.end = end;
        tag.self_closing = false;
        while let Some(c) = self.peek() {
            if self.ascii_run(&mut tag.name, true, None)? {
                continue;
            }
            if ws(c) || c == '/' || c == '>' {
                break;
            }
            self.take()?;
            self.name(&mut tag.name, c)?;
        }
        let mut count = 0usize;
        loop {
            self.budget.step()?;
            match self.peek() {
                Some(c) if ws(c) => {
                    self.take()?;
                }
                Some('>') => {
                    self.take()?;
                    break;
                }
                Some('/') => {
                    self.take()?;
                    if self.peek() == Some('>') {
                        self.take()?;
                        tag.self_closing = true;
                        break;
                    }
                    if self.peek().is_none() {
                        self.error();
                        return Ok(None);
                    }
                    self.error();
                }
                None => {
                    self.error();
                    return Ok(None);
                }
                _ => {
                    count += 1;
                    if count > self.budget.limits.attributes_per_tag {
                        return Err(ParseError::AttributesPerTag);
                    }
                    self.budget.attribute()?;
                    let mut attr = match self.spare_attributes.iter_mut().find_map(Option::take) {
                        Some(attribute) => attribute,
                        None => Attribute {
                            name: BudgetString::new(&self.budget.memory)?,
                            value: BudgetString::new(&self.budget.memory)?,
                        },
                    };
                    if self.peek() == Some('=') {
                        self.take()?;
                        self.error();
                        self.name(&mut attr.name, '=')?;
                    }
                    while let Some(c) = self.peek() {
                        if self.ascii_run(&mut attr.name, true, None)? {
                            continue;
                        }
                        if ws(c) || matches!(c, '/' | '>' | '=') {
                            break;
                        }
                        self.take()?;
                        if matches!(c, '\'' | '"' | '<') {
                            self.error();
                        }
                        self.name(&mut attr.name, c)?;
                    }
                    let mut duplicate = false;
                    for a in &tag.attributes {
                        self.budget.step()?;
                        if a.name == attr.name {
                            duplicate = true;
                            break;
                        }
                    }
                    if duplicate {
                        self.error();
                    }
                    while self.peek().is_some_and(ws) {
                        self.take()?;
                    }
                    if self.peek() == Some('=') {
                        self.take()?;
                        while self.peek().is_some_and(ws) {
                            self.take()?;
                        }
                        let quote = match self.peek() {
                            Some(c @ ('\'' | '"')) => {
                                self.take()?;
                                Some(c)
                            }
                            _ => None,
                        };
                        loop {
                            if self.ascii_run(&mut attr.value, false, quote)? {
                                continue;
                            }
                            let Some(c) = self.peek() else {
                                self.error();
                                return Ok(None);
                            };
                            if quote == Some(c) {
                                self.take()?;
                                if self.peek().is_some_and(|c| !ws(c) && c != '/' && c != '>') {
                                    self.error();
                                }
                                break;
                            }
                            if quote.is_none() && (ws(c) || c == '>') {
                                if attr.value.is_empty() {
                                    self.error();
                                }
                                break;
                            }
                            self.take()?;
                            if quote.is_none() && matches!(c, '"' | '\'' | '<' | '=' | '`') {
                                self.error();
                            }
                            if c == '&' {
                                self.reference(&mut attr.value, true)?;
                            } else {
                                self.value(&mut attr.value, c)?;
                            }
                        }
                    }
                    if !duplicate {
                        tag.attributes.try_push(attr)?;
                    }
                }
            }
        }
        if end {
            if tag.self_closing {
                self.error();
            }
            if !tag.attributes.is_empty() {
                self.error();
            }
            tag.attributes.clear();
            tag.self_closing = false;
        } else {
            self.last_start.clear();
            self.budget.append_text(&mut self.last_start, &tag.name)?;
        }
        Ok(Some(Token::Tag(tag)))
    }
    fn bogus_comment(&mut self) -> Result<Option<Token>, ParseError> {
        let mut out = BudgetString::new(&self.budget.memory)?;
        while let Some(c) = self.take()? {
            if c == '>' {
                break;
            }
            if c == '\0' {
                self.error();
            }
            self.append_char(&mut out, replace_null(c))?;
        }
        Ok(Some(Token::Comment(out)))
    }
    fn cdata(&mut self) -> Result<Option<Token>, ParseError> {
        let mut out = BudgetString::new(&self.budget.memory)?;
        while self.peek().is_some() {
            if self.rest().starts_with("]]>") {
                self.consume(3)?;
                return Ok(Some(Token::Text(out)));
            }
            if let Some(c) = self.take()? {
                self.append_char(&mut out, c)?;
            }
        }
        self.error();
        Ok(Some(Token::Text(out)))
    }
    fn comment(&mut self) -> Result<Option<Token>, ParseError> {
        // HTML comment states: start, start-dash, data, less-than, bang,
        // bang-dash, bang-dash-dash, end-dash, end, end-bang.
        let mut state = 0u8;
        let mut out = BudgetString::new(&self.budget.memory)?;
        let mut pending = None;
        loop {
            self.budget.step()?;
            let c = if let Some(c) = pending.take() {
                Some(c)
            } else {
                self.take()?
            };
            let Some(c) = c else {
                self.error();
                break;
            };
            match state {
                0 => match c {
                    '-' => state = 1,
                    '>' => {
                        self.error();
                        break;
                    }
                    _ => {
                        state = 2;
                        pending = Some(c);
                    }
                },
                1 => match c {
                    '-' => state = 8,
                    '>' => {
                        self.error();
                        break;
                    }
                    _ => {
                        self.append_char(&mut out, '-')?;
                        state = 2;
                        pending = Some(c);
                    }
                },
                2 => match c {
                    '<' => {
                        self.append_char(&mut out, '<')?;
                        state = 3;
                    }
                    '-' => state = 7,
                    _ => {
                        if c == '\0' {
                            self.error();
                        }
                        self.append_char(&mut out, replace_null(c))?;
                    }
                },
                3 => match c {
                    '!' => {
                        self.append_char(&mut out, '!')?;
                        state = 4;
                    }
                    '<' => self.append_char(&mut out, '<')?,
                    _ => {
                        state = 2;
                        pending = Some(c);
                    }
                },
                4 => {
                    if c == '-' {
                        state = 5;
                    } else {
                        state = 2;
                        pending = Some(c);
                    }
                }
                5 => {
                    if c == '-' {
                        state = 6;
                    } else {
                        state = 7;
                        pending = Some(c);
                    }
                }
                6 => {
                    if c != '>' {
                        self.error();
                    }
                    state = 8;
                    pending = Some(c);
                }
                7 => {
                    if c == '-' {
                        state = 8;
                    } else {
                        self.append_char(&mut out, '-')?;
                        state = 2;
                        pending = Some(c);
                    }
                }
                8 => match c {
                    '>' => break,
                    '!' => state = 9,
                    '-' => self.append_char(&mut out, '-')?,
                    _ => {
                        self.budget.append_text(&mut out, "--")?;
                        state = 2;
                        pending = Some(c);
                    }
                },
                9 => match c {
                    '-' => {
                        self.budget.append_text(&mut out, "--!")?;
                        state = 7;
                    }
                    '>' => {
                        self.error();
                        break;
                    }
                    _ => {
                        self.budget.append_text(&mut out, "--!")?;
                        state = 2;
                        pending = Some(c);
                    }
                },
                _ => return Err(ParseError::State),
            }
        }
        Ok(Some(Token::Comment(out)))
    }
    fn spaces(&mut self) -> Result<bool, ParseError> {
        let mut any = false;
        while self.peek().is_some_and(ws) {
            self.take()?;
            any = true;
        }
        Ok(any)
    }
    fn doctype(&mut self) -> Result<Option<Token>, ParseError> {
        let mut d = Doctype {
            name: BudgetString::new(&self.budget.memory)?,
            public_id: None,
            system_id: None,
            force_quirks: false,
        };
        if !self.spaces()? && self.peek().is_some_and(|c| c != '>') {
            self.error();
        }
        loop {
            match self.peek() {
                Some(c) if !ws(c) && c != '>' => {
                    self.take()?;
                    self.name(&mut d.name, c)?;
                }
                _ => break,
            }
        }
        if d.name.is_empty() {
            if self.peek().is_some() {
                self.error();
            }
            d.force_quirks = true;
        }
        self.spaces()?;
        if self.peek() == Some('>') {
            self.take()?;
            return Ok(Some(Token::Doctype(d)));
        }
        if self.peek().is_none() {
            self.error();
            d.force_quirks = true;
            return Ok(Some(Token::Doctype(d)));
        }
        let public = self.starts("PUBLIC");
        let system = self.starts("SYSTEM");
        if !public && !system {
            self.error();
            d.force_quirks = true;
            self.discard_doctype()?;
            return Ok(Some(Token::Doctype(d)));
        }
        self.consume(6)?;
        if !self.spaces()? && matches!(self.peek(), Some('\'' | '"')) {
            self.error();
        }
        let Some((value, closed)) = self.identifier(&mut d)? else {
            return Ok(Some(Token::Doctype(d)));
        };
        if public {
            d.public_id = Some(value);
        } else {
            d.system_id = Some(value);
        }
        if closed {
            return Ok(Some(Token::Doctype(d)));
        }
        let separated = self.spaces()?;
        match self.peek() {
            Some('>') => {
                self.take()?;
            }
            None => {
                self.error();
                d.force_quirks = true;
            }
            Some('\'' | '"') if public => {
                if !separated {
                    self.error();
                }
                if let Some((value, closed)) = self.identifier(&mut d)? {
                    d.system_id = Some(value);
                    if closed {
                        return Ok(Some(Token::Doctype(d)));
                    }
                    self.spaces()?;
                    match self.peek() {
                        Some('>') => {
                            self.take()?;
                        }
                        None => {
                            self.error();
                            d.force_quirks = true;
                        }
                        _ => {
                            self.error();
                            self.discard_doctype()?;
                        }
                    }
                }
            }
            _ => {
                self.error();
                if public {
                    d.force_quirks = true;
                }
                self.discard_doctype()?;
            }
        }
        Ok(Some(Token::Doctype(d)))
    }
    fn identifier(&mut self, d: &mut Doctype) -> Result<Option<(BudgetString, bool)>, ParseError> {
        let quote = match self.peek() {
            Some(c @ ('\'' | '"')) => {
                self.take()?;
                c
            }
            _ => {
                self.error();
                d.force_quirks = true;
                self.discard_doctype()?;
                return Ok(None);
            }
        };
        let mut out = BudgetString::new(&self.budget.memory)?;
        loop {
            match self.take()? {
                Some(c) if c == quote => return Ok(Some((out, false))),
                Some('>') | None => {
                    self.error();
                    d.force_quirks = true;
                    return Ok(Some((out, true)));
                }
                Some(c) => {
                    if c == '\0' {
                        self.error();
                    }
                    self.append_char(&mut out, replace_null(c))?;
                }
            }
        }
    }
    fn discard_doctype(&mut self) -> Result<(), ParseError> {
        while let Some(c) = self.take()? {
            if c == '>' {
                break;
            }
            if c == '\0' {
                self.error();
            }
        }
        Ok(())
    }
}
pub(crate) fn ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}
fn replace_null(c: char) -> char {
    if c == '\0' {
        '\u{fffd}'
    } else {
        c
    }
}

fn noncharacter(n: u32) -> bool {
    (0xfdd0..=0xfdef).contains(&n) || (n <= 0x10ffff && n & 0xffff >= 0xfffe)
}
fn control(n: u32) -> bool {
    n <= 0x1f || (0x7f..=0x9f).contains(&n)
}
