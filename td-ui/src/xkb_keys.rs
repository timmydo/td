//! Keycodes, aliases, symbol declarations and real modifier assignments.

use crate::xkb::{self, Diagnostic, Result};
use crate::xkb_symbols::{self as symbols, Symbol};
use crate::xkb_syntax::{self as syntax, Kind, Token};
use std::collections::{BTreeMap, BTreeSet};

const MAX_KEYS: usize = 768;

#[derive(Clone, Debug, Default)]
pub(crate) struct Key {
    pub name: String,
    pub symbols: Vec<Symbol>,
    pub typ: Option<String>,
    pub repeat: Option<bool>,
    pub virtuals: Option<Vec<String>>,
    pub modifier: u32,
    pub unsupported: Option<Diagnostic>,
}

impl Key {
    pub fn error(&self, reason: &'static str) -> Diagnostic {
        Diagnostic {
            offset: 0,
            item: format!("<{}>", self.name),
            reason,
        }
    }
    pub fn used(&self) -> bool {
        self.modifier != 0 || self.symbols.iter().any(Symbol::supported)
    }
    pub fn inferred_type(&self) -> Result<&str> {
        if let Some(name) = &self.typ {
            return Ok(name);
        }
        let alpha = |offset: usize| {
            self.symbols
                .get(offset)
                .and_then(|s| s.value)
                .and_then(symbols::ascii)
                .is_some_and(|c| c.is_ascii_lowercase())
                && self
                    .symbols
                    .get(offset + 1)
                    .and_then(|s| s.value)
                    .and_then(symbols::ascii)
                    .is_some_and(|c| c.is_ascii_uppercase())
        };
        let keypad = self
            .symbols
            .iter()
            .take(2)
            .any(|s| s.value.is_some_and(|v| (0xff80..=0xffbd).contains(&v)));
        Ok(match self.symbols.len() {
            0 | 1 => "ONE_LEVEL",
            2 if alpha(0) => "ALPHABETIC",
            2 if keypad => "KEYPAD",
            2 => "TWO_LEVEL",
            3 | 4 if alpha(0) && alpha(2) => "FOUR_LEVEL_ALPHABETIC",
            3 | 4 if alpha(0) => "FOUR_LEVEL_SEMIALPHABETIC",
            3 | 4 if keypad => "FOUR_LEVEL_KEYPAD",
            3 | 4 => "FOUR_LEVEL",
            _ => return Err(self.error("more than four symbols require an explicit type")),
        })
    }
}

pub(crate) fn parse(
    codes: &[Token<'_>],
    symbols: &[Token<'_>],
    types: &xkb::TypeCatalog,
) -> Result<BTreeMap<u32, Key>> {
    let names = keycodes(codes)?;
    let mut keys = BTreeMap::new();
    let mut defaults = Key::default();
    let mut modmaps = Vec::new();
    for statement in syntax::statements(symbols)? {
        let first = statement
            .first()
            .ok_or_else(|| syntax::error("empty symbols statement"))?;
        if first.is("key") && statement.get(1).is_some_and(|t| t.kind == Kind::Key) {
            let (header, body) = syntax::block(statement)?;
            let name = match header {
                [kind, name] if kind.is("key") && name.kind == Kind::Key => name,
                _ => return Err(first.error("invalid key declaration")),
            };
            let code = *names
                .get(name.text)
                .ok_or_else(|| name.error("key has no keycode"))?;
            let mut key = defaults.clone();
            key.name = name.text.to_owned();
            let mut fields = BTreeSet::new();
            for field in syntax::split(body, ",") {
                property(&mut key, field, &mut fields)?;
            }
            while key.symbols.last().is_some_and(|s| s.value == Some(0)) {
                key.symbols.pop();
            }
            if keys.insert(code, key).is_some() {
                return Err(name.error("duplicate symbol definition for keycode"));
            }
        } else if first.is("modifier_map") {
            let (header, body) = syntax::block(statement)?;
            let modifier = match header {
                [kind, name] if kind.is("modifier_map") => xkb::real_modifier(name.text)
                    .filter(|mask| mask.count_ones() == 1)
                    .ok_or_else(|| name.error("modifier_map requires one real modifier"))?,
                _ => return Err(first.error("invalid modifier_map")),
            };
            for target in syntax::split(body, ",") {
                match target {
                    [key] if key.kind == Kind::Key => modmaps.push((
                        modifier,
                        Some(
                            *names
                                .get(key.text)
                                .ok_or_else(|| key.error("unknown modifier key"))?,
                        ),
                        None,
                    )),
                    _ => modmaps.push((modifier, None, Some(Symbol::parse(target)?))),
                }
                if modmaps.len() > MAX_KEYS * 2 {
                    return Err(first.error("too many modifier assignments"));
                }
            }
        } else if first.is("virtual_modifiers") {
            // Declarations are compiled by TypeCatalog from this same token set.
        } else if first.is("name") {
            let (left, right) = syntax::assignment(statement)?;
            group_one(syntax::index(left)?)?;
            match right {
                [label] => {
                    label.string()?;
                }
                _ => return Err(first.error("invalid group name")),
            }
        } else if first.is("key") && statement.get(1).is_some_and(|t| t.is(".")) {
            let field = statement
                .get(2..)
                .ok_or_else(|| first.error("missing key default"))?;
            property(&mut defaults, field, &mut BTreeSet::new())?;
        } else {
            return Err(first.error("unsupported symbols statement"));
        }
    }
    // Modifier-map symbol lookup sees only the levels retained by the type.
    for key in keys.values_mut() {
        if let Some(name) = key.inferred_type().ok().map(str::to_owned) {
            key.typ = Some(name.clone());
            if let Some(levels) = types.levels(&name) {
                key.symbols.truncate(levels);
                key.symbols.resize_with(levels, || Symbol {
                    name: "NoSymbol".to_owned(),
                    value: Some(0),
                });
            }
        }
    }
    for (modifier, direct, symbol) in modmaps {
        let code = if let Some(code) = direct {
            code
        } else {
            let symbol = symbol.ok_or_else(|| syntax::error("missing modifier target"))?;
            keys.iter()
                .filter_map(|(code, key)| {
                    key.symbols
                        .iter()
                        .position(|s| s.matches(&symbol))
                        .map(|level| (level, *code))
                })
                .min()
                .map(|(_, code)| code)
                .ok_or_else(|| symbol.error("modifier_map", "unbound modifier keysym"))?
        };
        let key = keys
            .get_mut(&code)
            .ok_or_else(|| syntax::error("modifier key has no symbols"))?;
        if key.modifier != 0 && key.modifier != modifier {
            return Err(key.error("conflicting real modifier assignments"));
        }
        key.modifier = modifier;
    }
    Ok(keys)
}

fn keycodes(tokens: &[Token<'_>]) -> Result<BTreeMap<String, u32>> {
    let mut names = BTreeMap::new();
    let mut aliases = BTreeMap::new();
    let mut used_codes = BTreeSet::new();
    let mut minimum = None;
    let mut maximum = None;
    for statement in syntax::statements(tokens)? {
        let first = statement
            .first()
            .ok_or_else(|| syntax::error("empty keycode statement"))?;
        match statement {
            [name, equal, number] if name.kind == Kind::Key && equal.is("=") => {
                let code = integer(&[*number])?;
                if code < 8 || !used_codes.insert(code) {
                    return Err(name.error("invalid or duplicate keycode"));
                }
                if names.insert(name.text.to_owned(), code).is_some() {
                    return Err(name.error("duplicate key name"));
                }
            }
            [kind, name, equal, target]
                if kind.is("alias")
                    && name.kind == Kind::Key
                    && equal.is("=")
                    && target.kind == Kind::Key =>
            {
                if aliases
                    .insert(name.text.to_owned(), target.text.to_owned())
                    .is_some()
                {
                    return Err(name.error("duplicate alias"));
                }
            }
            [name, equal, number]
                if equal.is("=") && (name.is("minimum") || name.is("maximum")) =>
            {
                let bound = if name.is("minimum") {
                    &mut minimum
                } else {
                    &mut maximum
                };
                if bound.replace(integer(&[*number])?).is_some() {
                    return Err(name.error("duplicate keycode bound"));
                }
            }
            [kind, index, equal, label] if kind.is("indicator") && equal.is("=") => {
                if !(1..=32).contains(&integer(&[*index])?) {
                    return Err(index.error("indicator index exceeds 32"));
                }
                label.string()?;
            }
            _ => return Err(first.error("unsupported keycode statement")),
        }
        if names.len() + aliases.len() > MAX_KEYS {
            return Err(first.error("more than 768 keycode names and aliases"));
        }
    }
    let low = minimum.unwrap_or(8);
    let high = maximum.unwrap_or(u32::MAX);
    if low < 8 || low > high || names.values().any(|code| *code < low || *code > high) {
        return Err(syntax::error("keycode outside declared range"));
    }
    let mut resolved = BTreeMap::new();
    for alias in aliases.keys() {
        if names.contains_key(alias) {
            return Err(syntax::error("alias shadows a key name"));
        }
        let mut target = alias;
        let mut visited = BTreeSet::new();
        let code = loop {
            if let Some(code) = names.get(target) {
                break *code;
            }
            if !visited.insert(target) {
                return Err(Diagnostic {
                    offset: 0,
                    item: alias.clone(),
                    reason: "cyclic key alias",
                });
            }
            target = aliases.get(target).ok_or_else(|| Diagnostic {
                offset: 0,
                item: target.clone(),
                reason: "unknown alias target",
            })?;
        };
        resolved.insert(alias.clone(), code);
    }
    names.extend(resolved);
    Ok(names)
}

fn property(key: &mut Key, tokens: &[Token<'_>], fields: &mut BTreeSet<String>) -> Result<()> {
    let first = tokens
        .first()
        .ok_or_else(|| key.error("empty key property"))?;
    if first.is("[") {
        if !fields.insert("symbols".to_owned()) {
            return Err(first.error("additional layout group or duplicate symbols"));
        }
        key.symbols = symbol_list(tokens)?;
        return Ok(());
    }
    let (left, right) = syntax::assignment(tokens)?;
    let name = first.text.to_ascii_lowercase();
    if !fields.insert(name.clone()) {
        return Err(first.error("duplicate key property"));
    }
    if matches!(name.as_str(), "type" | "symbols" | "actions") && left.len() != 1 {
        group_one(syntax::index(left)?)?;
    }
    match name.as_str() {
        "type" => match right {
            [label] => key.typ = Some(label.string()?),
            _ => return Err(first.error("invalid key type")),
        },
        "symbols" => key.symbols = symbol_list(right)?,
        "repeat" if left.len() == 1 => key.repeat = Some(boolean(right)?),
        "virtualmodifiers" if left.len() == 1 => key.virtuals = Some(modifier_names(right)?),
        _ => {
            key.unsupported
                .get_or_insert_with(|| first.error("unsupported key property"));
        }
    }
    Ok(())
}

fn symbol_list(tokens: &[Token<'_>]) -> Result<Vec<Symbol>> {
    let body = syntax::group(tokens, "[", "]")?;
    let mut symbols = Vec::new();
    for symbol in syntax::split(body, ",") {
        if symbols.len() == 16 {
            return Err(syntax::error("more than 16 symbol levels"));
        }
        symbols.push(Symbol::parse(symbol)?);
    }
    Ok(symbols)
}

pub(crate) fn group_one(tokens: &[Token<'_>]) -> Result<()> {
    match tokens {
        [token] if token.is("Group1") || token.is("1") => Ok(()),
        _ => Err(tokens.first().map_or_else(
            || syntax::error("missing group"),
            |t| t.error("additional layout groups are unsupported"),
        )),
    }
}

pub(crate) fn integer(tokens: &[Token<'_>]) -> Result<u32> {
    match tokens {
        [token] if token.kind == Kind::Word => {
            xkb::number(token.text).ok_or_else(|| token.error("expected unsigned integer"))
        }
        _ => Err(syntax::error("expected unsigned integer")),
    }
}

pub(crate) fn boolean(tokens: &[Token<'_>]) -> Result<bool> {
    match tokens {
        [token] if token.is("true") || token.is("yes") || token.is("on") => Ok(true),
        [token] if token.is("false") || token.is("no") || token.is("off") => Ok(false),
        _ => Err(syntax::error("expected boolean")),
    }
}

pub(crate) fn modifier_names(tokens: &[Token<'_>]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for term in syntax::split(tokens, "+") {
        match term {
            [token] if token.kind == Kind::Word => names.push(token.text.to_owned()),
            _ => return Err(syntax::error("expected modifier sum")),
        }
    }
    if names.is_empty() || tokens.last().is_some_and(|t| t.is("+")) {
        return Err(syntax::error("expected modifier sum"));
    }
    Ok(names)
}
