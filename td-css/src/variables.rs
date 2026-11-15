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
/// One computed custom property. Its stored bytes are kept beside it, so an
/// override need not walk the value it replaces.
#[derive(Debug, PartialEq, Eq, Hash)]
pub(crate) struct Entry {
    pub(crate) value: CustomValue,
    bytes: usize,
}
impl Entry {
    const INVALID: Self = Self {
        value: None,
        bytes: 0,
    };
    fn new(value: CustomValue) -> Self {
        let bytes = value.as_ref().map_or(0, |tokens| stored_bytes(tokens));
        Self { value, bytes }
    }
    fn try_clone(&self) -> Result<Self, Error> {
        Ok(Self {
            value: copy_value(Some(&self.value))?,
            bytes: self.bytes,
        })
    }
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CustomProperties {
    pub(crate) entries: BudgetShared<BudgetMap<Text, Entry>>,
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
            .and_then(|entry| entry.value.as_ref().map(|tokens| &***tokens))
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
    /// Inherited entries copied into a changed map, outside the charges.
    pub copies: usize,
}
impl Budget {
    pub(crate) fn new(memory: &td_alloc::Budget) -> Result<Self, Error> {
        Ok(Self {
            memory: memory.try_clone()?,
            components: 0,
            bytes: 0,
            copies: 0,
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
/// A dependency: its name's length, charged on each visit, and its position
/// among the element's own names, if it is one.
type Edge = (usize, Option<usize>);
/// Each local name's dependencies, by position, in name order.
type Graph = BudgetVec<BudgetVec<Edge>>;
/// A name's position in a sorted map, by binary search.
fn position<V>(map: &BudgetMap<Text, V>, key: &str) -> Option<usize> {
    let (mut low, mut high) = (0, map.len());
    while low < high {
        let middle = low + (high - low) / 2;
        match map.get_index(middle)?.0.as_str().cmp(key) {
            std::cmp::Ordering::Less => low = middle + 1,
            std::cmp::Ordering::Greater => high = middle,
            std::cmp::Ordering::Equal => return Some(middle),
        }
    }
    None
}
/// References in token order; the caller sorts and dedups them once, so the
/// order they arrive in costs nothing more.
fn dependencies(
    values: &[Component],
    out: &mut BudgetVec<Text>,
    budget: &mut Budget,
) -> Result<(), Error> {
    for c in values {
        budget.charge(own_bytes(c))?;
        match c {
            Component::Function { name, values } => {
                if name.eq_ignore_ascii_case("var") {
                    if let Some((name, _)) = args(values) {
                        out.try_push(syntax::text(&budget.memory, name)?)?;
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
    // Everything below is indexed by a name's position in `local`, so the
    // order names are declared, referenced or visited in costs nothing more.
    let mut graph = td_alloc::BudgetVec::with_capacity(&memory, local.len())?;
    for value in local.values() {
        let mut names = BudgetVec::new(&memory)?;
        if let CustomSpecified::Tokens(tokens) = value {
            dependencies(tokens, &mut names, budget)?;
        }
        names.sort_unstable();
        names.dedup();
        let mut edges = td_alloc::BudgetVec::with_capacity(&memory, names.len())?;
        for name in names.iter() {
            edges.try_push((name.len(), position(&local, name)))?;
        }
        graph.try_push(edges)?;
    }
    let cycles = cycles(&local, &graph, budget)?;
    let mut slots = td_alloc::BudgetVec::with_capacity(&memory, local.len())?;
    for _ in 0..local.len() {
        slots.try_push(None)?;
    }
    let mut resolver = Resolver {
        local: &local,
        graph: &graph,
        inherited: &inherited,
        resolved: slots,
        depth: 0,
        cycles,
        budget,
    };
    for index in 0..local.len() {
        resolver.resolve(index)?;
    }
    // In name order, so each insertion appends.
    let mut resolved = td_alloc::BudgetMap::with_capacity(&memory, local.len())?;
    for (name, entry) in local.keys().zip(resolver.resolved) {
        resolved.try_insert(name.try_clone()?, entry.ok_or(Error::InvalidContext)?)?;
    }
    let mut bytes = inherited.bytes;
    for name in resolved.keys() {
        if let Some(old) = inherited.entries.get(name) {
            bytes = bytes
                .checked_sub(name.len() + old.bytes)
                .ok_or(Error::InvalidContext)?;
        }
    }
    for (name, entry) in &resolved {
        bytes = bytes
            .checked_add(name.len() + entry.bytes)
            .filter(|n| *n <= MAX_CUSTOM_BYTES)
            .ok_or(Error::Limit("MAX_CUSTOM_BYTES"))?;
    }
    if resolved
        .iter()
        .all(|(name, entry)| inherited.entries.get(name) == Some(entry))
    {
        return Ok(inherited);
    }
    // Merge the two sorted maps, so each entry lands at the end.
    let mut entries = td_alloc::BudgetMap::with_capacity(&memory, count)?;
    let mut fresh = resolved.into_iter().peekable();
    let mut copies = 0usize;
    for (name, old) in inherited.entries.iter() {
        while let Some((key, entry)) = fresh.next_if(|(key, _)| key < name) {
            entries.try_insert(key, entry)?;
        }
        if let Some((key, entry)) = fresh.next_if(|(key, _)| key == name) {
            entries.try_insert(key, entry)?;
            continue;
        }
        entries.try_insert(name.try_clone()?, old.try_clone()?)?;
        copies += 1;
    }
    for (key, entry) in fresh {
        entries.try_insert(key, entry)?;
    }
    budget.copies = budget.copies.saturating_add(copies);
    Ok(CustomProperties {
        entries: BudgetShared::new(&memory, entries)?,
        bytes,
    })
}
struct Resolver<'a> {
    local: &'a BudgetMap<Text, CustomSpecified>,
    graph: &'a Graph,
    inherited: &'a CustomProperties,
    /// By position in `local`.
    resolved: BudgetVec<Option<Entry>>,
    depth: usize,
    cycles: BudgetVec<bool>,
    budget: &'a mut Budget,
}
impl<'a> Resolver<'a> {
    fn inherited(&self, name: &str) -> Result<Entry, Error> {
        self.inherited
            .entries
            .get(name)
            .map_or(Ok(Entry::INVALID), Entry::try_clone)
    }
    fn resolve(&mut self, index: usize) -> Result<(), Error> {
        let local = self.local;
        let (name, raw) = local.get_index(index).ok_or(Error::InvalidContext)?;
        self.budget.charge(name.len())?;
        if self
            .resolved
            .get(index)
            .ok_or(Error::InvalidContext)?
            .is_some()
        {
            return Ok(());
        }
        let entry = if self.cycles.get(index).copied().unwrap_or(false) {
            Entry::INVALID
        } else {
            self.compute(index, name, raw)?
        };
        *self.resolved.get_mut(index).ok_or(Error::InvalidContext)? = Some(entry);
        Ok(())
    }
    fn compute(
        &mut self,
        index: usize,
        name: &'a Text,
        raw: &'a CustomSpecified,
    ) -> Result<Entry, Error> {
        let entry = match raw {
            CustomSpecified::Invalid => Entry::INVALID,
            CustomSpecified::Inherit => self.inherited(name)?,
            CustomSpecified::Tokens(tokens) => {
                if self.depth == MAX_VARIABLE_DEPTH {
                    return Err(Error::Limit("MAX_VARIABLE_DEPTH"));
                }
                self.depth += 1;
                let graph = self.graph;
                for &(bytes, target) in graph.get(index).ok_or(Error::InvalidContext)?.iter() {
                    match target {
                        Some(target) => self.resolve(target)?,
                        None => self.budget.charge(bytes)?,
                    }
                }
                let resolved = &self.resolved;
                let inherited = &self.inherited.entries;
                let local = self.local;
                let expanded = expand(tokens, 0, self.budget, &|key| {
                    position(local, key)
                        .and_then(|at| resolved.get(at))
                        .and_then(Option::as_ref)
                        .or_else(|| inherited.get(key))
                        .and_then(|entry| entry.value.as_ref().map(|tokens| &***tokens))
                })?;
                let result = match expanded {
                    None => Entry::INVALID,
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
                            Some("initial" | "revert" | "revert-layer") => Entry::INVALID,
                            Some("inherit" | "unset") => inherited
                                .get(name)
                                .map_or(Ok(Entry::INVALID), Entry::try_clone)?,
                            _ => Entry::new(Some(BudgetShared::new(&self.budget.memory, tokens)?)),
                        }
                    }
                };
                self.depth -= 1;
                result
            }
        };
        Ok(entry)
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
            .and_then(|entry| entry.value.as_ref().map(|tokens| &***tokens))
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

const UNVISITED: usize = usize::MAX;
struct Scc {
    indices: BudgetVec<usize>,
    low: BudgetVec<usize>,
    next: usize,
    stack: BudgetVec<usize>,
    on_stack: BudgetVec<bool>,
    cyclic: BudgetVec<bool>,
}
/// Which local names lie on a reference cycle, by position.
fn cycles(
    local: &BudgetMap<Text, CustomSpecified>,
    graph: &Graph,
    budget: &mut Budget,
) -> Result<BudgetVec<bool>, Error> {
    let count = local.len();
    let mut state = Scc {
        indices: BudgetVec::filled(&budget.memory, count, UNVISITED)?,
        low: BudgetVec::filled(&budget.memory, count, UNVISITED)?,
        next: 0,
        stack: BudgetVec::new(&budget.memory)?,
        on_stack: BudgetVec::filled(&budget.memory, count, false)?,
        cyclic: BudgetVec::filled(&budget.memory, count, false)?,
    };
    for node in 0..count {
        if state.indices.get(node) == Some(&UNVISITED) {
            visit(node, local, graph, &mut state, budget, 0)?;
        }
    }
    Ok(state.cyclic)
}
fn set<T>(values: &mut BudgetVec<T>, at: usize, value: T) -> Result<(), Error> {
    *values.get_mut(at).ok_or(Error::InvalidContext)? = value;
    Ok(())
}
fn visit(
    node: usize,
    local: &BudgetMap<Text, CustomSpecified>,
    graph: &Graph,
    state: &mut Scc,
    budget: &mut Budget,
    depth: usize,
) -> Result<(), Error> {
    if depth == MAX_VARIABLE_DEPTH {
        return Err(Error::Limit("MAX_VARIABLE_DEPTH"));
    }
    let (name, _) = local.get_index(node).ok_or(Error::InvalidContext)?;
    budget.charge(name.len())?;
    let index = state.next;
    state.next += 1;
    let mut low = index;
    set(&mut state.indices, node, index)?;
    state.stack.try_push(node)?;
    set(&mut state.on_stack, node, true)?;
    let edges = graph.get(node).ok_or(Error::InvalidContext)?;
    for &(bytes, target) in edges.iter() {
        budget.charge(bytes)?;
        let Some(dep) = target else {
            continue;
        };
        let seen = *state.indices.get(dep).ok_or(Error::InvalidContext)?;
        if seen == UNVISITED {
            visit(dep, local, graph, state, budget, depth + 1)?;
            low = low.min(*state.low.get(dep).ok_or(Error::InvalidContext)?);
        } else if state.on_stack.get(dep) == Some(&true) {
            low = low.min(seen);
        }
    }
    set(&mut state.low, node, low)?;
    if low == index {
        // The component is the top of the stack, down to this node.
        let start = state
            .stack
            .iter()
            .rposition(|member| *member == node)
            .ok_or(Error::InvalidContext)?;
        let cyclic =
            state.stack.len() - start > 1 || edges.iter().any(|&(_, target)| target == Some(node));
        while state.stack.len() > start {
            let member = state.stack.pop().ok_or(Error::InvalidContext)?;
            set(&mut state.on_stack, member, false)?;
            if cyclic {
                set(&mut state.cyclic, member, true)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
// Inline so its generated fixtures stay outside the scanned test file.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod entry_tests {
    use crate::test_tree::{tree, Handle};
    use crate::{ComputeStats, Origin, StyleSet};
    #[test]
    fn overrides_keep_entry_bytes_and_changed_maps_count_their_copies() {
        let nodes = tree();
        let element = |index| Handle {
            nodes: &nodes,
            index,
        };
        let set = StyleSet::new(&[(
            Origin::Author,
            &format!(
                "div{{--a:{};--b:2;--c:3}}p{{--a:1;--d:4}}",
                "x ".repeat(500)
            ),
        )])
        .unwrap();
        let parent = set.compute(&element(2), None, 16.0, "").unwrap();
        let mut stats = ComputeStats::default();
        let child = set
            .compute_with_stats(&element(3), Some(&parent), 16.0, "", &mut stats)
            .unwrap();
        // The override replaces the inherited value's bytes; only --b and --c
        // are copied from the parent.
        assert_eq!(stats.custom_copies, 2);
        let small = "--a".len() + 2 + 2 * ("--b".len() + 2);
        assert_eq!(child.custom.bytes, small + "--d".len() + 2);
        assert_eq!(
            child
                .custom
                .entries
                .keys()
                .map(|k| k.as_str())
                .collect::<Vec<_>>(),
            ["--a", "--b", "--c", "--d"]
        );
        let unchanged = set
            .compute_with_stats(&element(4), Some(&parent), 16.0, "", &mut stats)
            .unwrap();
        assert_eq!(stats.custom_copies, 0);
        assert!(std::ptr::eq(
            &*unchanged.custom.entries,
            &*parent.custom.entries
        ));
    }
}
