//! HTML selector parsing, specificity and budgeted matching.
use crate::syntax::{self, Component, Number, Token};
use crate::Error;
use std::collections::BTreeSet;

pub const MAX_SELECTORS: usize = 256;
pub const MAX_SELECTOR_PARTS: usize = 1024;
pub const MAX_SELECTOR_DEPTH: usize = 8;
pub const MAX_COMPOUNDS: usize = 64;
pub const MAX_MATCH_STEPS: usize = 150_000;
pub const MAX_ELEMENT_BYTES: usize = 65_536;
pub const MAX_NTH_COEFFICIENT: i64 = 1_000_000_000;

/// Caller-reported interaction state. Browsing history is deliberately absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Hover,
    Focus,
    Checked,
}

/// A cheap handle into a stable HTML tree. Siblings and children are elements
/// only. Attribute names supplied by the matcher are ASCII lowercase.
pub trait Element: Clone {
    fn tag_name(&self) -> &str;
    fn attribute(&self, name: &str) -> Option<&str>;
    fn parent(&self) -> Option<Self>;
    fn previous_sibling(&self) -> Option<Self>;
    fn next_sibling(&self) -> Option<Self>;
    fn first_child(&self) -> Option<Self>;
    /// Any nonempty text child (including whitespace); comments do not count.
    fn has_text(&self) -> bool;
    /// Host-language link semantics, independent of browsing history.
    fn is_link(&self) -> bool;
    /// Stable for one operation; the caller owns host-language state semantics.
    fn state(&self, _state: State) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    pub ids: u32,
    pub classes: u32,
    pub types: u32,
}

impl Specificity {
    fn add(self, other: Self) -> Self {
        Self {
            ids: self.ids.saturating_add(other.ids),
            classes: self.classes.saturating_add(other.classes),
            types: self.types.saturating_add(other.types),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SelectorList {
    selectors: Vec<Complex>,
}

#[derive(Clone, Debug)]
struct Complex {
    parts: Vec<Part>,
    specificity: Specificity,
}

#[derive(Clone, Debug)]
struct Part {
    simples: Vec<Simple>,
    relation: Option<Combinator>,
}

#[derive(Clone, Copy, Debug)]
enum Combinator {
    Descendant,
    Child,
    Adjacent,
    Subsequent,
}

#[derive(Clone, Debug)]
enum Simple {
    Universal,
    Type(String),
    Id(String),
    Class(String),
    Attribute(Attribute),
    Root,
    Empty,
    Link,
    Visited,
    State(State),
    Nth {
        a: i64,
        b: i64,
        reverse: bool,
        of_type: bool,
    },
    Only {
        of_type: bool,
    },
    Logical {
        kind: Logical,
        list: SelectorList,
    },
}

#[derive(Clone, Copy, Debug)]
enum Logical {
    Is,
    Where,
    Not,
}

#[derive(Clone, Debug)]
struct Attribute {
    name: String,
    operator: Operator,
    value: String,
    insensitive: bool,
}

#[derive(Clone, Copy, Debug)]
enum Operator {
    Present,
    Equal,
    Word,
    Dash,
    Prefix,
    Suffix,
    Substring,
}

impl Simple {
    fn specificity(&self) -> Specificity {
        match self {
            Self::Universal => Specificity::default(),
            Self::Type(_) => Specificity {
                types: 1,
                ..Specificity::default()
            },
            Self::Id(_) => Specificity {
                ids: 1,
                ..Specificity::default()
            },
            Self::Logical {
                kind: Logical::Where,
                ..
            } => Specificity::default(),
            Self::Logical { list, .. } => list.specificities().max().unwrap_or_default(),
            _ => Specificity {
                classes: 1,
                ..Specificity::default()
            },
        }
    }
}

#[derive(Default)]
struct CompileBudget {
    members: usize,
    parts: usize,
}

impl CompileBudget {
    fn member(&mut self) -> Result<(), Error> {
        if self.members == MAX_SELECTORS {
            return Err(Error::Limit("MAX_SELECTORS"));
        }
        self.members += 1;
        Ok(())
    }

    fn part(&mut self) -> Result<(), Error> {
        if self.parts == MAX_SELECTOR_PARTS {
            return Err(Error::Limit("MAX_SELECTOR_PARTS"));
        }
        self.parts += 1;
        Ok(())
    }
}

pub(crate) struct MatchBudget {
    remaining: usize,
}

impl MatchBudget {
    pub(crate) fn new() -> Self {
        Self {
            remaining: MAX_MATCH_STEPS,
        }
    }

    fn step(&mut self) -> Result<(), Error> {
        self.charge(1)
    }

    pub(crate) fn charge(&mut self, amount: usize) -> Result<(), Error> {
        self.remaining = self
            .remaining
            .checked_sub(amount)
            .ok_or(Error::Limit("MAX_MATCH_STEPS"))?;
        Ok(())
    }

    pub(crate) fn used(&self) -> usize {
        MAX_MATCH_STEPS - self.remaining
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum IndexKey {
    Id(String),
    Class(String),
    Tag(String),
    Universal,
}

fn compound_keys(part: &Part) -> BTreeSet<IndexKey> {
    for kind in 0..3 {
        for simple in &part.simples {
            let key = match (kind, simple) {
                (0, Simple::Id(s)) => IndexKey::Id(s.clone()),
                (1, Simple::Class(s)) => IndexKey::Class(s.clone()),
                (2, Simple::Type(s)) => IndexKey::Tag(s.clone()),
                _ => continue,
            };
            return [key].into_iter().collect();
        }
    }
    for simple in &part.simples {
        if let Simple::Logical {
            kind: Logical::Is | Logical::Where,
            list,
        } = simple
        {
            return list.index_keys();
        }
    }
    [IndexKey::Universal].into_iter().collect()
}

impl SelectorList {
    /// Each matching complex selector must satisfy one of these keys.
    pub(crate) fn index_keys(&self) -> BTreeSet<IndexKey> {
        let mut keys = BTreeSet::new();
        for selector in &self.selectors {
            if let Some(part) = selector.parts.last() {
                keys.extend(compound_keys(part));
            }
        }
        if keys.contains(&IndexKey::Universal) {
            return [IndexKey::Universal].into_iter().collect();
        }
        keys
    }
    pub fn parse(input: &str) -> Result<Self, Error> {
        let components = syntax::parse_components(input)?;
        Self::from_components(components.value())
    }

    pub(crate) fn from_components(values: &[Component]) -> Result<Self, Error> {
        parse_list(values, false, 0, &mut CompileBudget::default())
    }

    pub fn specificities(&self) -> impl Iterator<Item = Specificity> + '_ {
        self.selectors.iter().map(|s| s.specificity)
    }

    pub fn matching_specificity<E: Element>(
        &self,
        element: &E,
    ) -> Result<Option<Specificity>, Error> {
        self.matches_with(element, &mut MatchBudget::new())
    }

    pub(crate) fn matches_with<E: Element>(
        &self,
        element: &E,
        budget: &mut MatchBudget,
    ) -> Result<Option<Specificity>, Error> {
        let mut best = None;
        for selector in &self.selectors {
            if matches_complex(&selector.parts, element, budget)? {
                best = Some(best.map_or(selector.specificity, |old: Specificity| {
                    old.max(selector.specificity)
                }));
            }
        }
        Ok(best)
    }
}

fn parse_list(
    values: &[Component],
    forgiving: bool,
    depth: usize,
    budget: &mut CompileBudget,
) -> Result<SelectorList, Error> {
    if forgiving && !any_value(values) {
        return Err(Error::InvalidSelector);
    }
    let mut selectors = Vec::new();
    for member in values.split(|v| v.delim(',')) {
        budget.member()?;
        match parse_complex(syntax::trim(member), depth, budget) {
            Ok(selector) => selectors.push(selector),
            Err(Error::InvalidSelector) if forgiving => {}
            Err(error) => return Err(error),
        }
    }
    Ok(SelectorList { selectors })
}

fn any_value(values: &[Component]) -> bool {
    values.iter().all(|value| match value {
        Component::Token(Token::BadString | Token::BadUrl | Token::Close(_)) => false,
        Component::Block { values, .. } | Component::Function { values, .. } => any_value(values),
        _ => true,
    })
}

struct Cursor<'a> {
    values: &'a [Component],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(values: &'a [Component]) -> Self {
        Self { values, pos: 0 }
    }
    fn peek(&self) -> Option<&'a Component> {
        self.values.get(self.pos)
    }
    fn take(&mut self) -> Option<&'a Component> {
        let value = self.peek()?;
        self.pos += 1;
        Some(value)
    }
    fn space(&mut self) -> bool {
        let before = self.pos;
        while self.peek().is_some_and(Component::whitespace) {
            self.pos += 1;
        }
        before != self.pos
    }
    fn end(&self) -> bool {
        self.peek().is_none()
    }
}

fn parse_complex(
    values: &[Component],
    depth: usize,
    budget: &mut CompileBudget,
) -> Result<Complex, Error> {
    let mut cursor = Cursor::new(values);
    let mut parts = Vec::new();
    let mut specificity = Specificity::default();
    let mut relation = None;
    loop {
        if parts.len() == MAX_COMPOUNDS {
            return Err(Error::Limit("MAX_COMPOUNDS"));
        }
        let mut simples = Vec::new();
        match cursor.peek() {
            Some(Component::Token(Token::Ident(name))) => {
                budget.part()?;
                simples.push(Simple::Type(name.to_ascii_lowercase()));
                let _ = cursor.take();
            }
            Some(value) if value.delim('*') => {
                budget.part()?;
                simples.push(Simple::Universal);
                let _ = cursor.take();
            }
            _ => {}
        }
        loop {
            let simple = match cursor.peek() {
                Some(Component::Token(Token::Hash { value, id: true })) => {
                    budget.part()?;
                    let _ = cursor.take();
                    Simple::Id(value.clone())
                }
                Some(value) if value.delim('.') => {
                    budget.part()?;
                    let _ = cursor.take();
                    let name = cursor
                        .take()
                        .and_then(Component::ident)
                        .ok_or(Error::InvalidSelector)?;
                    Simple::Class(name.to_owned())
                }
                Some(Component::Block { kind: '[', values }) => {
                    budget.part()?;
                    let _ = cursor.take();
                    Simple::Attribute(parse_attribute(values)?)
                }
                Some(value) if value.delim(':') => {
                    budget.part()?;
                    let _ = cursor.take();
                    pseudo(&mut cursor, depth, budget)?
                }
                _ => break,
            };
            simples.push(simple);
        }
        if simples.is_empty() {
            return Err(Error::InvalidSelector);
        }
        for simple in &simples {
            specificity = specificity.add(simple.specificity());
        }
        parts.push(Part { simples, relation });
        let spaced = cursor.space();
        if cursor.end() {
            break;
        }
        relation = match cursor.peek() {
            Some(value) if value.delim('>') => Some(Combinator::Child),
            Some(value) if value.delim('+') => Some(Combinator::Adjacent),
            Some(value) if value.delim('~') => Some(Combinator::Subsequent),
            _ if spaced => Some(Combinator::Descendant),
            _ => return Err(Error::InvalidSelector),
        };
        budget.part()?;
        if !matches!(relation, Some(Combinator::Descendant)) {
            let _ = cursor.take();
        }
        cursor.space();
    }
    Ok(Complex { parts, specificity })
}

fn parse_attribute(values: &[Component]) -> Result<Attribute, Error> {
    let mut cursor = Cursor::new(syntax::trim(values));
    let name = cursor
        .take()
        .and_then(Component::ident)
        .ok_or(Error::InvalidSelector)?
        .to_ascii_lowercase();
    cursor.space();
    if cursor.end() {
        return Ok(Attribute {
            name,
            operator: Operator::Present,
            value: String::new(),
            insensitive: false,
        });
    }
    let operator = match cursor.take() {
        Some(value) if value.delim('=') => Operator::Equal,
        Some(Component::Token(Token::Delim(ch))) => {
            let operator = match ch {
                '~' => Operator::Word,
                '|' => Operator::Dash,
                '^' => Operator::Prefix,
                '$' => Operator::Suffix,
                '*' => Operator::Substring,
                _ => return Err(Error::InvalidSelector),
            };
            if !cursor.take().is_some_and(|v| v.delim('=')) {
                return Err(Error::InvalidSelector);
            }
            operator
        }
        _ => return Err(Error::InvalidSelector),
    };
    cursor.space();
    let value = match cursor.take() {
        Some(Component::Token(Token::Ident(value) | Token::String(value))) => value.clone(),
        _ => return Err(Error::InvalidSelector),
    };
    cursor.space();
    let insensitive = match cursor.take() {
        None => false,
        Some(item) if item.ident().is_some_and(|n| n.eq_ignore_ascii_case("i")) => true,
        Some(item) if item.ident().is_some_and(|n| n.eq_ignore_ascii_case("s")) => false,
        _ => return Err(Error::InvalidSelector),
    };
    cursor.space();
    if !cursor.end() {
        return Err(Error::InvalidSelector);
    }
    Ok(Attribute {
        name,
        operator,
        value,
        insensitive,
    })
}

fn pseudo(
    cursor: &mut Cursor<'_>,
    depth: usize,
    budget: &mut CompileBudget,
) -> Result<Simple, Error> {
    match cursor.take() {
        Some(Component::Token(Token::Ident(name))) => {
            let name = name.to_ascii_lowercase();
            Ok(match name.as_str() {
                "root" => Simple::Root,
                "empty" => Simple::Empty,
                "link" => Simple::Link,
                "visited" => Simple::Visited,
                "hover" => Simple::State(State::Hover),
                "focus" => Simple::State(State::Focus),
                "checked" => Simple::State(State::Checked),
                "first-child" | "last-child" | "first-of-type" | "last-of-type" => Simple::Nth {
                    a: 0,
                    b: 1,
                    reverse: name.starts_with("last"),
                    of_type: name.ends_with("of-type"),
                },
                "only-child" | "only-of-type" => Simple::Only {
                    of_type: name.ends_with("of-type"),
                },
                _ => return Err(Error::InvalidSelector),
            })
        }
        Some(Component::Function { name, values }) => {
            let name = name.to_ascii_lowercase();
            match name.as_str() {
                "is" | "where" | "not" => {
                    if depth == MAX_SELECTOR_DEPTH {
                        return Err(Error::Limit("MAX_SELECTOR_DEPTH"));
                    }
                    let kind = match name.as_str() {
                        "is" => Logical::Is,
                        "where" => Logical::Where,
                        _ => Logical::Not,
                    };
                    let list =
                        parse_list(values, !matches!(kind, Logical::Not), depth + 1, budget)?;
                    Ok(Simple::Logical { kind, list })
                }
                "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
                    let (a, b) = nth(values)?;
                    Ok(Simple::Nth {
                        a,
                        b,
                        reverse: name.contains("last"),
                        of_type: name.ends_with("of-type"),
                    })
                }
                _ => Err(Error::InvalidSelector),
            }
        }
        _ => Err(Error::InvalidSelector),
    }
}

fn coefficient(text: &str) -> Result<i64, Error> {
    let value = text
        .parse::<i64>()
        .map_err(|_| Error::Limit("MAX_NTH_COEFFICIENT"))?;
    if !(-MAX_NTH_COEFFICIENT..=MAX_NTH_COEFFICIENT).contains(&value) {
        return Err(Error::Limit("MAX_NTH_COEFFICIENT"));
    }
    Ok(value)
}

fn integer(value: &Component, signless: bool) -> Result<i64, Error> {
    match value {
        Component::Token(Token::Number(Number {
            representation,
            integer: true,
        })) if !signless || representation.starts_with(|c: char| c.is_ascii_digit()) => {
            coefficient(representation)
        }
        _ => Err(Error::InvalidSelector),
    }
}

// Syntax §6.2, expressed in tokens so whitespace cannot glue "2 n" into 2n.
fn nth(values: &[Component]) -> Result<(i64, i64), Error> {
    let mut cursor = Cursor::new(syntax::trim(values));
    let plus = cursor.peek().is_some_and(|v| v.delim('+'));
    if plus {
        let _ = cursor.take();
    }
    let first = cursor.take().ok_or(Error::InvalidSelector)?;
    let (a, unit) = match first {
        Component::Token(Token::Ident(name)) => {
            let name = name.to_ascii_lowercase();
            if !plus && (name == "odd" || name == "even") {
                if !cursor.end() {
                    return Err(Error::InvalidSelector);
                }
                return Ok((2, i64::from(name == "odd")));
            }
            if let Some(unit) = name.strip_prefix("-n").filter(|_| !plus) {
                (-1, unit.to_owned())
            } else if let Some(unit) = name.strip_prefix('n') {
                (1, unit.to_owned())
            } else {
                return Err(Error::InvalidSelector);
            }
        }
        Component::Token(Token::Dimension(number, name)) if !plus && number.integer => {
            let name = name.to_ascii_lowercase();
            let unit = name.strip_prefix('n').ok_or(Error::InvalidSelector)?;
            (coefficient(&number.representation)?, unit.to_owned())
        }
        _ if !plus && cursor.end() => return Ok((0, integer(first, false)?)),
        _ => return Err(Error::InvalidSelector),
    };
    cursor.space();
    if let Some(digits) = unit.strip_prefix('-').filter(|s| !s.is_empty()) {
        if !digits.bytes().all(|c| c.is_ascii_digit()) || !cursor.end() {
            return Err(Error::InvalidSelector);
        }
        return Ok((a, -coefficient(digits)?));
    }
    let b = if unit == "-" {
        integer(cursor.take().ok_or(Error::InvalidSelector)?, true)?
            .checked_neg()
            .ok_or(Error::InvalidSelector)?
    } else if unit.is_empty() {
        match cursor.take() {
            None => 0,
            Some(Component::Token(Token::Delim(sign @ ('+' | '-')))) => {
                cursor.space();
                let value = integer(cursor.take().ok_or(Error::InvalidSelector)?, true)?;
                if *sign == '-' {
                    -value
                } else {
                    value
                }
            }
            Some(value @ Component::Token(Token::Number(number)))
                if number.representation.starts_with(['+', '-']) =>
            {
                integer(value, false)?
            }
            _ => return Err(Error::InvalidSelector),
        }
    } else {
        return Err(Error::InvalidSelector);
    };
    cursor.space();
    if !cursor.end() {
        return Err(Error::InvalidSelector);
    }
    Ok((a, b))
}

pub(crate) fn checked<'a>(text: &'a str, budget: &mut MatchBudget) -> Result<&'a str, Error> {
    if text.len() > MAX_ELEMENT_BYTES {
        return Err(Error::Limit("MAX_ELEMENT_BYTES"));
    }
    budget.charge(text.len())?;
    Ok(text)
}

fn attribute<'a, E: Element>(
    element: &'a E,
    name: &str,
    budget: &mut MatchBudget,
) -> Result<Option<&'a str>, Error> {
    element
        .attribute(name)
        .map(|s| checked(s, budget))
        .transpose()
}

pub(crate) fn html_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

impl Attribute {
    fn matches(&self, actual: &str) -> bool {
        let equals = |a: &str, b: &str| {
            if self.insensitive {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        let value = self.value.as_str();
        match self.operator {
            Operator::Present => true,
            Operator::Equal => equals(actual, value),
            Operator::Word => {
                !value.is_empty() && actual.split(html_space).any(|word| equals(word, value))
            }
            Operator::Dash => {
                equals(actual, value)
                    || (actual.get(..value.len()).is_some_and(|s| equals(s, value))
                        && actual.as_bytes().get(value.len()) == Some(&b'-'))
            }
            Operator::Prefix => {
                !value.is_empty() && actual.get(..value.len()).is_some_and(|s| equals(s, value))
            }
            Operator::Suffix => {
                !value.is_empty()
                    && actual
                        .len()
                        .checked_sub(value.len())
                        .and_then(|n| actual.get(n..))
                        .is_some_and(|s| equals(s, value))
            }
            Operator::Substring => {
                !value.is_empty()
                    && if self.insensitive {
                        actual
                            .to_ascii_lowercase()
                            .contains(&value.to_ascii_lowercase())
                    } else {
                        actual.contains(value)
                    }
            }
        }
    }
}

fn matches_complex<E: Element>(
    parts: &[Part],
    element: &E,
    budget: &mut MatchBudget,
) -> Result<bool, Error> {
    budget.step()?;
    let Some((part, previous)) = parts.split_last() else {
        return Ok(false);
    };
    for simple in &part.simples {
        if !matches_simple(simple, element, budget)? {
            return Ok(false);
        }
    }
    let Some(relation) = part.relation else {
        return Ok(true);
    };
    let mut current = element.clone();
    loop {
        budget.step()?;
        let next = match relation {
            Combinator::Descendant | Combinator::Child => current.parent(),
            Combinator::Adjacent | Combinator::Subsequent => current.previous_sibling(),
        };
        let Some(next) = next else {
            return Ok(false);
        };
        if matches_complex(previous, &next, budget)? {
            return Ok(true);
        }
        if matches!(relation, Combinator::Child | Combinator::Adjacent) {
            return Ok(false);
        }
        current = next;
    }
}

fn position<E: Element>(
    element: &E,
    reverse: bool,
    of_type: bool,
    budget: &mut MatchBudget,
) -> Result<i64, Error> {
    let tag = if of_type {
        Some(checked(element.tag_name(), budget)?)
    } else {
        None
    };
    let mut current = element.clone();
    let mut count = 1i64;
    loop {
        budget.step()?;
        let next = if reverse {
            current.next_sibling()
        } else {
            current.previous_sibling()
        };
        let Some(next) = next else {
            return Ok(count);
        };
        if match tag {
            Some(tag) => checked(next.tag_name(), budget)?.eq_ignore_ascii_case(tag),
            None => true,
        } {
            count += 1;
        }
        current = next;
    }
}

fn matches_simple<E: Element>(
    simple: &Simple,
    element: &E,
    budget: &mut MatchBudget,
) -> Result<bool, Error> {
    budget.step()?;
    Ok(match simple {
        Simple::Universal => true,
        Simple::Type(name) => checked(element.tag_name(), budget)?.eq_ignore_ascii_case(name),
        Simple::Id(name) => attribute(element, "id", budget)? == Some(name.as_str()),
        Simple::Class(name) => attribute(element, "class", budget)?
            .is_some_and(|s| s.split(html_space).any(|c| c == name)),
        Simple::Attribute(test) => {
            budget.charge(test.value.len())?;
            attribute(element, &test.name, budget)?.is_some_and(|value| test.matches(value))
        }
        Simple::Root => {
            budget.step()?;
            element.parent().is_none()
        }
        Simple::Empty => {
            budget.step()?;
            element.first_child().is_none() && !element.has_text()
        }
        Simple::Link => element.is_link(),
        Simple::Visited => false,
        Simple::State(state) => element.state(*state),
        Simple::Logical { kind, list } => {
            let matches = list.matches_with(element, budget)?.is_some();
            if matches!(kind, Logical::Not) {
                !matches
            } else {
                matches
            }
        }
        Simple::Nth {
            a,
            b,
            reverse,
            of_type,
        } => {
            let index = position(element, *reverse, *of_type, budget)?;
            let delta = index - b;
            if *a == 0 {
                delta == 0
            } else {
                delta % a == 0 && delta / a >= 0
            }
        }
        Simple::Only { of_type } => {
            position(element, false, *of_type, budget)? == 1
                && position(element, true, *of_type, budget)? == 1
        }
    })
}

#[cfg(test)]
mod tests;
