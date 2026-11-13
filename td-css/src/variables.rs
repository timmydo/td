//! Computed custom properties and bounded token substitution.
use crate::syntax::Text;
use crate::syntax::{self, Component, Declaration, Token};
use crate::{Error, MAX_NESTING};
use td_alloc::{BudgetMap, BudgetShared, BudgetVec};

pub const MAX_CUSTOM_PROPERTIES: usize = 8192;
pub const MAX_CUSTOM_BYTES: usize = 1024 * 1024;
pub const MAX_VARIABLE_DEPTH: usize = 64;
pub const MAX_SUBSTITUTION_COMPONENTS: usize = 262_144;
pub const MAX_SUBSTITUTION_BYTES: usize = 4 * 1024 * 1024;

pub(crate) type Tokens = BudgetShared<BudgetVec<Component>>;
type CustomValue = Option<Tokens>;
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CustomProperties {
    pub(crate) entries: BudgetShared<BudgetMap<Text, CustomValue>>,
    bytes: usize,
}
impl CustomProperties {
    pub(crate) fn new(memory: &td_alloc::Budget) -> Result<Self, Error> {
        Ok(Self {
            entries: BudgetShared::new(memory, BudgetMap::new(memory)?)?,
            bytes: 0,
        })
    }
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            entries: self.entries.try_clone()?,
            bytes: self.bytes,
        })
    }
    pub(crate) fn get(&self, name: &str) -> Option<&[Component]> {
        self.entries
            .get(name)
            .and_then(|value| value.as_ref().map(|tokens| &***tokens))
    }
}
#[derive(Debug)]
pub(crate) enum CustomSpecified {
    Inherit,
    Invalid,
    Tokens(Tokens),
}
#[derive(Debug)]
pub(crate) struct CustomDeclaration {
    pub name: Text,
    pub value: CustomSpecified,
    pub important: bool,
}
#[derive(Debug)]
pub(crate) struct Pending {
    pub name: Text,
    pub tokens: Tokens,
}
pub(crate) struct Budget {
    pub(crate) memory: td_alloc::Budget,
    pub components: usize,
    pub bytes: usize,
}
impl Budget {
    pub(crate) fn new(memory: &td_alloc::Budget) -> Result<Self, Error> {
        Ok(Self {
            memory: memory.try_clone()?,
            components: 0,
            bytes: 0,
        })
    }
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        if self.components == MAX_SUBSTITUTION_COMPONENTS {
            return Err(Error::Limit("MAX_SUBSTITUTION_COMPONENTS"));
        }
        self.components += 1;
        let next = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= MAX_SUBSTITUTION_BYTES)
            .ok_or(Error::Limit("MAX_SUBSTITUTION_BYTES"))?;
        self.bytes = next;
        Ok(())
    }
}
pub(crate) fn is_name(name: &str) -> bool {
    name.starts_with("--") && name.len() > 2
}
fn args(values: &[Component]) -> Option<(&str, Option<&[Component]>)> {
    let comma = values.iter().position(|v| v.delim(','));
    let first = syntax::trim(values.get(..comma.unwrap_or(values.len()))?);
    let [name] = first else {
        return None;
    };
    let name = name.ident().filter(|n| is_name(n))?;
    Some((name, comma.and_then(|i| values.get(i + 1..))))
}
/// None means syntactically invalid; Some records whether var occurs.
pub(crate) fn inspect(values: &[Component]) -> Option<bool> {
    inspect_inner(values, true)
}
fn inspect_inner(values: &[Component], top: bool) -> Option<bool> {
    let mut found = false;
    for c in values {
        match c {
            Component::Token(Token::BadString | Token::BadUrl | Token::Close(_)) => return None,
            Component::Token(Token::Delim('!' | ';')) if top => return None,
            Component::Function { name, values } => {
                if name.eq_ignore_ascii_case("var") {
                    let (_, fallback) = args(values)?;
                    if let Some(fallback) = fallback {
                        let _ = inspect_inner(fallback, true)?;
                    }
                    found = true;
                } else {
                    found |= inspect_inner(values, false)?;
                }
            }
            Component::Block { values, .. } => found |= inspect_inner(values, false)?,
            _ => {}
        }
    }
    Some(found)
}
pub(crate) fn parse(
    declaration: &Declaration,
    memory: &td_alloc::Budget,
) -> Result<Option<CustomDeclaration>, Error> {
    if !is_name(declaration.name()) || inspect(declaration.value()).is_none() {
        return Ok(None);
    }
    let values = declaration.value();
    let wide = match values {
        [v] => v.ident(),
        _ => None,
    };
    let value = match wide.and_then(|v| {
        syntax::keyword(
            v,
            &["initial", "inherit", "unset", "revert", "revert-layer"],
        )
    }) {
        Some("initial") => CustomSpecified::Invalid,
        Some("inherit" | "unset") => CustomSpecified::Inherit,
        Some("revert" | "revert-layer") => return Ok(None),
        _ => CustomSpecified::Tokens(BudgetShared::new(
            memory,
            syntax::copy_components(values, memory)?,
        )?),
    };
    Ok(Some(CustomDeclaration {
        name: syntax::text(memory, declaration.name())?,
        value,
        important: declaration.important(),
    }))
}
impl CustomSpecified {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(match self {
            Self::Inherit => Self::Inherit,
            Self::Invalid => Self::Invalid,
            Self::Tokens(tokens) => Self::Tokens(tokens.try_clone()?),
        })
    }
}
impl Pending {
    pub(crate) fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            name: self.name.try_clone()?,
            tokens: self.tokens.try_clone()?,
        })
    }
}
fn copy_value(value: Option<&CustomValue>) -> Result<CustomValue, Error> {
    value
        .and_then(Option::as_ref)
        .map(|tokens| Ok(tokens.try_clone()?))
        .transpose()
}

fn own_bytes(c: &Component) -> usize {
    match c {
        Component::Token(t) => match t {
            Token::Ident(s)
            | Token::Function(s)
            | Token::AtKeyword(s)
            | Token::String(s)
            | Token::Url(s) => s.len() + 1,
            Token::Hash { value, .. } => value.len() + 1,
            Token::Number(n) | Token::Percentage(n) => n.representation.len() + 1,
            Token::Dimension(n, u) => n.representation.len() + u.len() + 1,
            _ => 1,
        },
        Component::Function { name, .. } => name.len() + 2,
        Component::Block { .. } => 2,
    }
}
fn stored_bytes(values: &[Component]) -> usize {
    values
        .iter()
        .map(|c| {
            own_bytes(c)
                + match c {
                    Component::Block { values, .. } | Component::Function { values, .. } => {
                        stored_bytes(values)
                    }
                    _ => 0,
                }
        })
        .sum()
}
fn dependencies(
    values: &[Component],
    out: &mut BudgetMap<Text, ()>,
    budget: &mut Budget,
) -> Result<(), Error> {
    for c in values {
        budget.charge(own_bytes(c))?;
        match c {
            Component::Function { name, values } => {
                if name.eq_ignore_ascii_case("var") {
                    if let Some((name, _)) = args(values) {
                        out.try_insert(syntax::text(&budget.memory, name)?, ())?;
                    }
                }
                dependencies(values, out, budget)?;
            }
            Component::Block { values, .. } => dependencies(values, out, budget)?,
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn compute(
    local: BudgetMap<Text, CustomSpecified>,
    parent: Option<&CustomProperties>,
    budget: &mut Budget,
) -> Result<CustomProperties, Error> {
    let memory = budget.memory.try_clone()?;
    let inherited = match parent {
        Some(parent) => parent.try_clone()?,
        None => CustomProperties::new(&memory)?,
    };
    if local.is_empty() {
        return Ok(inherited);
    }
    let count = inherited.entries.len()
        + local
            .keys()
            .filter(|name| !inherited.entries.contains_key(*name))
            .count();
    if count > MAX_CUSTOM_PROPERTIES {
        return Err(Error::Limit("MAX_CUSTOM_PROPERTIES"));
    }
    let mut graph = BudgetMap::new(&memory)?;
    for (name, value) in &local {
        let mut edges = BudgetMap::new(&memory)?;
        if let CustomSpecified::Tokens(tokens) = value {
            dependencies(tokens, &mut edges, budget)?;
        }
        graph.try_insert(name.try_clone()?, edges)?;
    }
    let cycles = cycles(&graph, budget)?;
    let mut resolver = Resolver {
        local: &local,
        graph: &graph,
        inherited: &inherited,
        resolved: BudgetMap::new(&memory)?,
        stack: BudgetVec::new(&memory)?,
        cycles,
        budget,
    };
    for name in local.keys() {
        let _ = resolver.resolve(name)?;
    }
    let mut bytes = inherited.bytes;
    for name in resolver.resolved.keys() {
        if let Some(old) = inherited.entries.get(name) {
            bytes = bytes
                .checked_sub(
                    name.len() + old.as_ref().map(|tokens| stored_bytes(tokens)).unwrap_or(0),
                )
                .ok_or(Error::InvalidContext)?;
        }
    }
    for (name, value) in &resolver.resolved {
        bytes = bytes
            .checked_add(
                name.len()
                    + value
                        .as_ref()
                        .map(|tokens| stored_bytes(tokens))
                        .unwrap_or(0),
            )
            .filter(|n| *n <= MAX_CUSTOM_BYTES)
            .ok_or(Error::Limit("MAX_CUSTOM_BYTES"))?;
    }
    if resolver
        .resolved
        .iter()
        .all(|(name, value)| inherited.entries.get(name) == Some(value))
    {
        return Ok(inherited);
    }
    let mut entries = BudgetMap::new(&memory)?;
    for (name, value) in inherited.entries.iter() {
        entries.try_insert(name.try_clone()?, copy_value(Some(value))?)?;
    }
    for (name, value) in resolver.resolved {
        entries.try_insert(name, value)?;
    }
    Ok(CustomProperties {
        entries: BudgetShared::new(&memory, entries)?,
        bytes,
    })
}
struct Resolver<'a> {
    local: &'a BudgetMap<Text, CustomSpecified>,
    graph: &'a BudgetMap<Text, BudgetMap<Text, ()>>,
    inherited: &'a CustomProperties,
    resolved: BudgetMap<Text, CustomValue>,
    stack: BudgetVec<Text>,
    cycles: BudgetMap<Text, ()>,
    budget: &'a mut Budget,
}
impl Resolver<'_> {
    fn resolve(&mut self, name: &Text) -> Result<CustomValue, Error> {
        self.budget.charge(name.len())?;
        if let Some(value) = self.resolved.get(name) {
            return copy_value(Some(value));
        }
        if self.cycles.contains_key(name) {
            self.resolved.try_insert(name.try_clone()?, None)?;
            return Ok(None);
        }
        let Some(raw) = self.local.get(name) else {
            return copy_value(self.inherited.entries.get(name));
        };
        let value = match raw {
            CustomSpecified::Invalid => None,
            CustomSpecified::Inherit => copy_value(self.inherited.entries.get(name))?,
            CustomSpecified::Tokens(tokens) => {
                if self.stack.len() == MAX_VARIABLE_DEPTH {
                    return Err(Error::Limit("MAX_VARIABLE_DEPTH"));
                }
                self.stack.try_push(name.try_clone()?)?;
                if let Some(deps) = self.graph.get(name) {
                    for dep in deps.keys() {
                        let _ = self.resolve(dep)?;
                    }
                }
                let resolved = &self.resolved;
                let inherited = &self.inherited.entries;
                let expanded = expand(tokens, 0, self.budget, &|key| {
                    resolved
                        .get(key)
                        .or_else(|| inherited.get(key))
                        .and_then(|value| value.as_ref().map(|tokens| &***tokens))
                })?;
                let result = match expanded {
                    None => None,
                    Some(tokens) => {
                        let wide = match syntax::trim(&tokens) {
                            [c] => c.ident(),
                            _ => None,
                        };
                        match wide.and_then(|v| {
                            syntax::keyword(
                                v,
                                &["initial", "revert", "revert-layer", "inherit", "unset"],
                            )
                        }) {
                            Some("initial" | "revert" | "revert-layer") => None,
                            Some("inherit" | "unset") => copy_value(inherited.get(name))?,
                            _ => Some(BudgetShared::new(&self.budget.memory, tokens)?),
                        }
                    }
                };
                let _ = self.stack.pop();
                result
            }
        };
        self.resolved
            .try_insert(name.try_clone()?, copy_value(Some(&value))?)?;
        Ok(value)
    }
}

pub(crate) fn substitute(
    values: &[Component],
    custom: &CustomProperties,
    budget: &mut Budget,
) -> Result<Option<BudgetVec<Component>>, Error> {
    expand(values, 0, budget, &|key| {
        custom
            .entries
            .get(key)
            .and_then(|value| value.as_ref().map(|tokens| &***tokens))
    })
}
fn copy_values(
    values: &[Component],
    depth: usize,
    budget: &mut Budget,
) -> Result<BudgetVec<Component>, Error> {
    let mut out = BudgetVec::new(&budget.memory)?;
    for c in values {
        budget.charge(own_bytes(c))?;
        let value = match c {
            Component::Block { kind, values } => {
                if depth == MAX_NESTING {
                    return Err(Error::Limit("MAX_NESTING"));
                }
                Component::Block {
                    kind: *kind,
                    values: copy_values(values, depth + 1, budget)?,
                }
            }
            Component::Function { name, values } => {
                if depth == MAX_NESTING {
                    return Err(Error::Limit("MAX_NESTING"));
                }
                Component::Function {
                    name: name.try_clone()?,
                    values: copy_values(values, depth + 1, budget)?,
                }
            }
            _ => c.try_clone()?,
        };
        out.try_push(value)?;
    }
    Ok(out)
}
fn expand<'a, F: Fn(&str) -> Option<&'a [Component]>>(
    values: &[Component],
    depth: usize,
    budget: &mut Budget,
    lookup: &F,
) -> Result<Option<BudgetVec<Component>>, Error> {
    let mut out = BudgetVec::new(&budget.memory)?;
    for c in values {
        budget.charge(own_bytes(c))?;
        match c {
            Component::Function { name, values } if name.eq_ignore_ascii_case("var") => {
                let Some((name, fallback)) = args(values) else {
                    return Ok(None);
                };
                budget.charge(name.len())?;
                if let Some(tokens) = lookup(name) {
                    for value in copy_values(tokens, depth, budget)? {
                        out.try_push(value)?;
                    }
                } else if let Some(fallback) = fallback {
                    if depth == MAX_NESTING {
                        return Err(Error::Limit("MAX_NESTING"));
                    }
                    let Some(expanded) = expand(fallback, depth + 1, budget, lookup)? else {
                        return Ok(None);
                    };
                    for value in expanded {
                        out.try_push(value)?;
                    }
                } else {
                    return Ok(None);
                }
            }
            Component::Function { name, values } => {
                if depth == MAX_NESTING {
                    return Err(Error::Limit("MAX_NESTING"));
                }
                let Some(values) = expand(values, depth + 1, budget, lookup)? else {
                    return Ok(None);
                };
                out.try_push(Component::Function {
                    name: name.try_clone()?,
                    values,
                })?;
            }
            Component::Block { kind, values } => {
                if depth == MAX_NESTING {
                    return Err(Error::Limit("MAX_NESTING"));
                }
                let Some(values) = expand(values, depth + 1, budget, lookup)? else {
                    return Ok(None);
                };
                out.try_push(Component::Block {
                    kind: *kind,
                    values,
                })?;
            }
            _ => out.try_push(c.try_clone()?)?,
        }
    }
    Ok(Some(out))
}

struct Scc {
    indices: BudgetMap<Text, usize>,
    low: BudgetMap<Text, usize>,
    stack: BudgetVec<Text>,
    on_stack: BudgetMap<Text, ()>,
    cyclic: BudgetMap<Text, ()>,
}
fn cycles(
    graph: &BudgetMap<Text, BudgetMap<Text, ()>>,
    budget: &mut Budget,
) -> Result<BudgetMap<Text, ()>, Error> {
    let mut state = Scc {
        indices: BudgetMap::new(&budget.memory)?,
        low: BudgetMap::new(&budget.memory)?,
        stack: BudgetVec::new(&budget.memory)?,
        on_stack: BudgetMap::new(&budget.memory)?,
        cyclic: BudgetMap::new(&budget.memory)?,
    };
    for name in graph.keys() {
        if !state.indices.contains_key(name) {
            visit(name, graph, &mut state, budget, 0)?;
        }
    }
    Ok(state.cyclic)
}
fn visit(
    name: &Text,
    graph: &BudgetMap<Text, BudgetMap<Text, ()>>,
    state: &mut Scc,
    budget: &mut Budget,
    depth: usize,
) -> Result<(), Error> {
    if depth == MAX_VARIABLE_DEPTH {
        return Err(Error::Limit("MAX_VARIABLE_DEPTH"));
    }
    budget.charge(name.len())?;
    let index = state.indices.len();
    let mut low = index;
    state.indices.try_insert(name.try_clone()?, index)?;
    state.stack.try_push(name.try_clone()?)?;
    state.on_stack.try_insert(name.try_clone()?, ())?;
    if let Some(edges) = graph.get(name) {
        for dep in edges.keys() {
            budget.charge(dep.len())?;
            if !graph.contains_key(dep) {
                continue;
            }
            if !state.indices.contains_key(dep) {
                visit(dep, graph, state, budget, depth + 1)?;
                low = low.min(state.low.get(dep).copied().ok_or(Error::InvalidContext)?);
            } else if state.on_stack.contains_key(dep) {
                low = low.min(
                    state
                        .indices
                        .get(dep)
                        .copied()
                        .ok_or(Error::InvalidContext)?,
                );
            }
        }
    }
    state.low.try_insert(name.try_clone()?, low)?;
    if low == index {
        let mut members = BudgetVec::new(&budget.memory)?;
        while let Some(member) = state.stack.pop() {
            state.on_stack.remove(&member);
            let done = member == *name;
            members.try_push(member)?;
            if done {
                break;
            }
        }
        if members.len() > 1
            || graph
                .get(name)
                .is_some_and(|edges| edges.contains_key(name))
        {
            for member in members {
                state.cyclic.try_insert(member, ())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
