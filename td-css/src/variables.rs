//! Computed custom properties and bounded token substitution.
use crate::syntax::{self, Component, Declaration, Token};
use crate::{Error, MAX_NESTING};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const MAX_CUSTOM_PROPERTIES: usize = 8192;
pub const MAX_CUSTOM_BYTES: usize = 1024 * 1024;
pub const MAX_VARIABLE_DEPTH: usize = 64;
pub const MAX_SUBSTITUTION_COMPONENTS: usize = 262_144;
pub const MAX_SUBSTITUTION_BYTES: usize = 4 * 1024 * 1024;

type Tokens = Arc<[Component]>;
type CustomValue = Option<Tokens>;
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CustomProperties {
    entries: Arc<BTreeMap<String, CustomValue>>,
    bytes: usize,
}
impl CustomProperties {
    pub(crate) fn get(&self, name: &str) -> Option<&[Component]> {
        self.entries.get(name).and_then(Option::as_deref)
    }
}
#[derive(Clone, Debug)]
pub(crate) enum CustomSpecified {
    Inherit,
    Invalid,
    Tokens(Tokens),
}
#[derive(Clone, Debug)]
pub(crate) struct CustomDeclaration {
    pub name: String,
    pub value: CustomSpecified,
    pub important: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct Pending {
    pub name: String,
    pub tokens: Tokens,
}
#[derive(Default)]
pub(crate) struct Budget {
    pub components: usize,
    pub bytes: usize,
}
impl Budget {
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
pub(crate) fn parse(declaration: &Declaration) -> Option<CustomDeclaration> {
    if !is_name(declaration.name()) {
        return None;
    }
    let values = declaration.value();
    let _ = inspect(values)?;
    let wide = match values {
        [v] => v.ident(),
        _ => None,
    };
    let value = match wide.map(str::to_ascii_lowercase).as_deref() {
        Some("initial") => CustomSpecified::Invalid,
        Some("inherit" | "unset") => CustomSpecified::Inherit,
        Some("revert" | "revert-layer") => return None,
        _ => CustomSpecified::Tokens(Arc::from(values)),
    };
    Some(CustomDeclaration {
        name: declaration.name().to_owned(),
        value,
        important: declaration.important(),
    })
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
    out: &mut BTreeSet<String>,
    budget: &mut Budget,
) -> Result<(), Error> {
    for c in values {
        budget.charge(own_bytes(c))?;
        match c {
            Component::Function { name, values } => {
                if name.eq_ignore_ascii_case("var") {
                    if let Some((name, _)) = args(values) {
                        out.insert(name.to_owned());
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
    local: BTreeMap<String, CustomSpecified>,
    parent: Option<&CustomProperties>,
    budget: &mut Budget,
) -> Result<CustomProperties, Error> {
    let inherited = parent.cloned().unwrap_or_default();
    if local.is_empty() {
        return Ok(inherited);
    }
    let mut count = inherited.entries.len();
    for name in local.keys() {
        if !inherited.entries.contains_key(name) {
            count += 1;
        }
    }
    if count > MAX_CUSTOM_PROPERTIES {
        return Err(Error::Limit("MAX_CUSTOM_PROPERTIES"));
    }
    let mut graph = BTreeMap::new();
    for (name, value) in &local {
        let mut edges = BTreeSet::new();
        if let CustomSpecified::Tokens(tokens) = value {
            dependencies(tokens, &mut edges, budget)?;
        }
        graph.insert(name.clone(), edges);
    }
    let cycles = cycles(&graph, budget)?;
    let mut resolver = Resolver {
        local: &local,
        graph: &graph,
        inherited: &inherited,
        resolved: BTreeMap::new(),
        stack: Vec::new(),
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
                .checked_sub(name.len() + old.as_deref().map(stored_bytes).unwrap_or(0))
                .ok_or(Error::InvalidContext)?;
        }
    }
    let mut changes = Vec::new();
    for (name, value) in resolver.resolved {
        bytes = bytes
            .checked_add(name.len() + value.as_deref().map(stored_bytes).unwrap_or(0))
            .filter(|n| *n <= MAX_CUSTOM_BYTES)
            .ok_or(Error::Limit("MAX_CUSTOM_BYTES"))?;
        changes.push((name, value));
    }
    let mut entries = (*inherited.entries).clone();
    for (name, value) in changes {
        entries.insert(name, value);
    }
    Ok(CustomProperties {
        entries: Arc::new(entries),
        bytes,
    })
}
struct Resolver<'a> {
    local: &'a BTreeMap<String, CustomSpecified>,
    graph: &'a BTreeMap<String, BTreeSet<String>>,
    inherited: &'a CustomProperties,
    resolved: BTreeMap<String, CustomValue>,
    stack: Vec<String>,
    cycles: BTreeSet<String>,
    budget: &'a mut Budget,
}
impl Resolver<'_> {
    fn resolve(&mut self, name: &str) -> Result<CustomValue, Error> {
        self.budget.charge(name.len())?;
        if let Some(value) = self.resolved.get(name) {
            return Ok(value.clone());
        }
        if self.cycles.contains(name) {
            self.resolved.insert(name.to_owned(), None);
            return Ok(None);
        }
        let Some(raw) = self.local.get(name) else {
            return Ok(self.inherited.entries.get(name).cloned().flatten());
        };
        let value = match raw {
            CustomSpecified::Invalid => None,
            CustomSpecified::Inherit => self.inherited.entries.get(name).cloned().flatten(),
            CustomSpecified::Tokens(tokens) => {
                if self.stack.len() == MAX_VARIABLE_DEPTH {
                    return Err(Error::Limit("MAX_VARIABLE_DEPTH"));
                }
                self.stack.push(name.to_owned());
                if let Some(deps) = self.graph.get(name) {
                    for dep in deps {
                        let _ = self.resolve(dep)?;
                    }
                }
                let result = {
                    let resolved = &self.resolved;
                    let inherited = &self.inherited.entries;
                    expand(tokens, 0, self.budget, &|key| {
                        resolved
                            .get(key)
                            .or_else(|| inherited.get(key))
                            .cloned()
                            .flatten()
                    })?
                    .and_then(|tokens| {
                        let wide = match syntax::trim(&tokens) {
                            [c] => c.ident(),
                            _ => None,
                        };
                        match wide.map(str::to_ascii_lowercase).as_deref() {
                            Some("initial" | "revert" | "revert-layer") => None,
                            Some("inherit" | "unset") => inherited.get(name).cloned().flatten(),
                            _ => Some(Arc::from(tokens)),
                        }
                    })
                };
                let _ = self.stack.pop();
                result
            }
        };
        self.resolved.insert(name.to_owned(), value.clone());
        Ok(value)
    }
}

pub(crate) fn substitute(
    values: &[Component],
    custom: &CustomProperties,
    budget: &mut Budget,
) -> Result<Option<Vec<Component>>, Error> {
    expand(values, 0, budget, &|key| {
        custom.entries.get(key).cloned().flatten()
    })
}
fn copy_values(
    values: &[Component],
    depth: usize,
    budget: &mut Budget,
) -> Result<Vec<Component>, Error> {
    let mut out = Vec::new();
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
                    name: name.clone(),
                    values: copy_values(values, depth + 1, budget)?,
                }
            }
            _ => c.clone(),
        };
        out.push(value);
    }
    Ok(out)
}
fn expand<F: Fn(&str) -> CustomValue>(
    values: &[Component],
    depth: usize,
    budget: &mut Budget,
    lookup: &F,
) -> Result<Option<Vec<Component>>, Error> {
    let mut out = Vec::new();
    for c in values {
        budget.charge(own_bytes(c))?;
        match c {
            Component::Function { name, values } if name.eq_ignore_ascii_case("var") => {
                let Some((name, fallback)) = args(values) else {
                    return Ok(None);
                };
                budget.charge(name.len())?;
                if let Some(tokens) = lookup(name) {
                    out.extend(copy_values(&tokens, depth, budget)?);
                } else if let Some(fallback) = fallback {
                    if depth == MAX_NESTING {
                        return Err(Error::Limit("MAX_NESTING"));
                    }
                    let Some(expanded) = expand(fallback, depth + 1, budget, lookup)? else {
                        return Ok(None);
                    };
                    out.extend(expanded);
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
                out.push(Component::Function {
                    name: name.clone(),
                    values,
                });
            }
            Component::Block { kind, values } => {
                if depth == MAX_NESTING {
                    return Err(Error::Limit("MAX_NESTING"));
                }
                let Some(values) = expand(values, depth + 1, budget, lookup)? else {
                    return Ok(None);
                };
                out.push(Component::Block {
                    kind: *kind,
                    values,
                });
            }
            _ => out.push(c.clone()),
        }
    }
    Ok(Some(out))
}

#[derive(Default)]
struct Scc {
    indices: BTreeMap<String, usize>,
    low: BTreeMap<String, usize>,
    stack: Vec<String>,
    on_stack: BTreeSet<String>,
    cyclic: BTreeSet<String>,
}
fn cycles(
    graph: &BTreeMap<String, BTreeSet<String>>,
    budget: &mut Budget,
) -> Result<BTreeSet<String>, Error> {
    let mut state = Scc::default();
    for name in graph.keys() {
        if !state.indices.contains_key(name) {
            visit(name, graph, &mut state, budget, 0)?;
        }
    }
    Ok(state.cyclic)
}
fn visit(
    name: &str,
    graph: &BTreeMap<String, BTreeSet<String>>,
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
    state.indices.insert(name.to_owned(), index);
    state.stack.push(name.to_owned());
    state.on_stack.insert(name.to_owned());
    if let Some(edges) = graph.get(name) {
        for dep in edges {
            budget.charge(dep.len())?;
            if !graph.contains_key(dep) {
                continue;
            }
            if !state.indices.contains_key(dep) {
                visit(dep, graph, state, budget, depth + 1)?;
                low = low.min(state.low.get(dep).copied().ok_or(Error::InvalidContext)?);
            } else if state.on_stack.contains(dep) {
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
    state.low.insert(name.to_owned(), low);
    if low == index {
        let mut members = Vec::new();
        while let Some(member) = state.stack.pop() {
            state.on_stack.remove(&member);
            let done = member == name;
            members.push(member);
            if done {
                break;
            }
        }
        if members.len() > 1 || graph.get(name).is_some_and(|edges| edges.contains(name)) {
            state.cyclic.extend(members);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
