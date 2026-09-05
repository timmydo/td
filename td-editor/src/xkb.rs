//! XKB type-table foundation, not a keyboard-activation validator.
//!
//! Parses the lexical envelope and type tables of a supplied text-v1 map.
//! Keycodes, symbols, compatibility interpretations and input translation
//! remain separate work: successful parsing here must never enable input.

use std::collections::BTreeMap;

use crate::xkb_syntax::{self as syntax, Kind, Token};

const MAX_TYPES: usize = 256;
const MAX_VIRTUALS: usize = 24;
const MAX_LEVELS: usize = 16;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub offset: usize,
    pub item: String,
    pub reason: &'static str,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "XKB byte {} ({:?}): {}",
            self.offset, self.item, self.reason
        )
    }
}

impl std::error::Error for Diagnostic {}
pub type Result<T> = std::result::Result<T, Diagnostic>;

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct Mask {
    real: u32,
    virtuals: u32,
}

impl Mask {
    fn subset(self, other: Self) -> bool {
        self.real & !other.real == 0 && self.virtuals & !other.virtuals == 0
    }

    fn resolve(self, bindings: &[(u32, u32)]) -> (u32, bool) {
        let mut real = self.real;
        let mut active = true;
        for &(bit, mask) in bindings {
            if self.virtuals & bit != 0 {
                real |= mask;
                active &= mask != 0;
            }
        }
        (real, active)
    }
}

#[derive(Debug)]
struct Virtual {
    bit: u32,
    encoding: Option<u32>,
}

#[derive(Debug, Default)]
struct Entry {
    order: usize,
    level: Option<usize>,
    preserve: Option<Mask>,
}

#[derive(Debug)]
struct Type {
    name: String,
    offset: usize,
    modifiers: Mask,
    entries: BTreeMap<Mask, Entry>,
    levels: usize,
    unsupported: Option<Diagnostic>,
}

impl Type {
    fn error(&self, reason: &'static str) -> Diagnostic {
        Diagnostic {
            offset: self.offset,
            item: self.name.clone(),
            reason,
        }
    }
}

/// Type declarations only. Other sections are lexically bounded, not
/// semantically validated. Never treat this as approval to accept key events.
#[derive(Debug)]
pub struct TypeCatalog {
    virtuals: BTreeMap<String, Virtual>,
    types: BTreeMap<String, Type>,
}

/// Real-mask encoding derived by a future keymap compiler from modifier_map
/// and compatibility interpretations, not a fixed Alt/NumLock bit guess.
#[derive(Clone, Copy, Debug)]
pub struct VirtualBinding<'a> {
    pub name: &'a str,
    pub mask: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    /// Zero-based symbol level. An unmatched state selects level zero.
    pub level: usize,
    /// XKB-mode consumed modifiers, including inactive bits in the type mask.
    pub consumed: u32,
}

#[derive(Debug)]
pub struct ResolvedType {
    mask: u32,
    levels: usize,
    entries: BTreeMap<u32, Selection>,
}

impl ResolvedType {
    pub fn levels(&self) -> usize {
        self.levels
    }

    /// State is the compositor's depressed | latched | locked real mask.
    /// No state is changed and no XKB action is executed.
    pub fn select(&self, state: u32) -> Selection {
        self.entries
            .get(&(state & self.mask))
            .copied()
            .unwrap_or(Selection {
                level: 0,
                consumed: self.mask,
            })
    }
}

impl TypeCatalog {
    pub fn parse(source: &str) -> Result<Self> {
        let tokens = syntax::lex(source)?;
        let mut outer = syntax::statements(&tokens)?;
        let map = outer
            .next()
            .ok_or_else(|| syntax::error("missing xkb_keymap"))?;
        if outer.next().is_some() {
            return Err(syntax::error("expected one xkb_keymap"));
        }
        let (header, body) = syntax::block(map)?;
        named_header(header, "xkb_keymap")?;
        let mut sections = BTreeMap::new();
        for statement in syntax::statements(body)? {
            let (header, body) = syntax::block(statement)?;
            let first = header
                .first()
                .ok_or_else(|| syntax::error("missing section"))?;
            let name = [
                "xkb_keycodes",
                "xkb_types",
                "xkb_compatibility",
                "xkb_symbols",
                "xkb_geometry",
            ]
            .into_iter()
            .find(|name| first.is(name))
            .ok_or_else(|| first.error("unsupported keymap section"))?;
            named_header(header, name)?;
            if sections.insert(name, body).is_some() {
                return Err(first.error("duplicate keymap section"));
            }
        }
        for name in [
            "xkb_keycodes",
            "xkb_types",
            "xkb_compatibility",
            "xkb_symbols",
        ] {
            if !sections.contains_key(name) {
                return Err(Diagnostic {
                    offset: 0,
                    item: name.to_owned(),
                    reason: "missing required section",
                });
            }
        }
        let mut catalog = Self {
            virtuals: BTreeMap::new(),
            types: BTreeMap::new(),
        };
        // Repeated virtual declarations across sections are normal serialized XKB.
        for name in ["xkb_types", "xkb_compatibility", "xkb_symbols"] {
            let section = sections
                .get(name)
                .ok_or_else(|| syntax::error("missing section"))?;
            for statement in syntax::statements(section)? {
                if statement.first().is_some_and(|t| t.is("virtual_modifiers")) {
                    catalog.declare_virtuals(
                        statement
                            .get(1..)
                            .ok_or_else(|| syntax::error("missing virtual modifiers"))?,
                    )?;
                }
            }
        }
        let types = sections
            .get("xkb_types")
            .ok_or_else(|| syntax::error("missing types"))?;
        for statement in syntax::statements(types)? {
            if statement.first().is_some_and(|t| t.is("virtual_modifiers")) {
                continue;
            }
            if catalog.types.len() == MAX_TYPES {
                return Err(syntax::error("more than 256 types"));
            }
            let typ = catalog.parse_type(statement)?;
            if catalog.types.contains_key(&typ.name) {
                return Err(typ.error("duplicate type"));
            }
            catalog.types.insert(typ.name.clone(), typ);
        }
        Ok(catalog)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.types.keys().map(String::as_str)
    }

    /// Unbound virtuals deactivate entries that require them; they must not
    /// collapse into a base-level entry. Explicit encodings cannot be replaced.
    /// Unsupported unused types are retained but refuse resolution by name.
    pub fn resolve(&self, name: &str, supplied: &[VirtualBinding<'_>]) -> Result<ResolvedType> {
        let typ = self.types.get(name).ok_or_else(|| Diagnostic {
            offset: 0,
            item: name.to_owned(),
            reason: "unknown type",
        })?;
        if let Some(error) = &typ.unsupported {
            return Err(error.clone());
        }
        if supplied.len() > MAX_VIRTUALS {
            return Err(typ.error("more than 24 virtual bindings"));
        }
        let mut bindings: BTreeMap<&str, u32> = BTreeMap::new();
        for binding in supplied {
            let declaration = self.virtuals.get(binding.name).ok_or_else(|| Diagnostic {
                offset: typ.offset,
                item: binding.name.to_owned(),
                reason: "undeclared virtual binding",
            })?;
            if declaration
                .encoding
                .is_some_and(|mask| mask != binding.mask)
            {
                return Err(typ.error("binding conflicts with explicit virtual encoding"));
            }
            if bindings.insert(binding.name, binding.mask).is_some() {
                return Err(typ.error("duplicate virtual binding"));
            }
        }
        let bindings: Vec<_> = self
            .virtuals
            .iter()
            .map(|(name, declaration)| {
                (
                    declaration.bit,
                    declaration
                        .encoding
                        .or_else(|| bindings.get(name.as_str()).copied())
                        .unwrap_or(0),
                )
            })
            .collect();
        let (mask, _) = typ.modifiers.resolve(&bindings);
        let mut entries = BTreeMap::new();
        let mut ordered: Vec<_> = typ.entries.iter().collect();
        ordered.sort_unstable_by_key(|(_, entry)| entry.order);
        for (modifiers, entry) in ordered {
            let (state, active) = modifiers.resolve(&bindings);
            if !active {
                continue;
            }
            let (preserve, _) = entry.preserve.unwrap_or_default().resolve(&bindings);
            let selection = Selection {
                level: entry.level.unwrap_or(0),
                consumed: mask & !preserve,
            };
            // Distinct virtual masks can alias the same real mask. XKB uses
            // the first declared active entry, not the order of modifier bits.
            entries.entry(state).or_insert(selection);
        }
        Ok(ResolvedType {
            mask,
            levels: typ.levels,
            entries,
        })
    }

    fn declare_virtuals(&mut self, tokens: &[Token<'_>]) -> Result<()> {
        if tokens.is_empty() {
            return Err(syntax::error("missing virtual modifiers"));
        }
        for item in syntax::split(tokens, ",") {
            let (name, encoding) = match item {
                [name] => (*name, None),
                [name, equal, rest @ ..] if equal.is("=") && !rest.is_empty() => {
                    (*name, Some(real_expression(rest)?))
                }
                _ => return Err(syntax::error("invalid virtual modifier declaration")),
            };
            if name.kind != Kind::Word
                || !name
                    .text
                    .starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                || real_modifier(name.text).is_some()
                || number(name.text).is_some()
            {
                return Err(name.error("invalid virtual modifier name"));
            }
            if let Some(previous) = self.virtuals.get_mut(name.text) {
                if let Some(encoding) = encoding {
                    if previous.encoding.is_some_and(|old| old != encoding) {
                        return Err(name.error("conflicting virtual modifier encodings"));
                    }
                    previous.encoding = Some(encoding);
                }
                continue;
            }
            if self.virtuals.len() == MAX_VIRTUALS {
                return Err(name.error("more than 24 virtual modifiers"));
            }
            let bit = 1u32
                .checked_shl(self.virtuals.len() as u32)
                .ok_or_else(|| name.error("virtual modifier overflow"))?;
            self.virtuals
                .insert(name.text.to_owned(), Virtual { bit, encoding });
        }
        Ok(())
    }

    fn expression(&self, tokens: &[Token<'_>]) -> Result<Mask> {
        let mut mask = Mask::default();
        for token in terms(tokens)? {
            if let Some(real) = real_modifier(token.text).or_else(|| number(token.text)) {
                if real > 0xff {
                    return Err(token.error("numeric type masks may name only the eight real bits"));
                }
                mask.real |= real;
            } else if let Some(virtual_mod) = self.virtuals.get(token.text) {
                mask.virtuals |= virtual_mod.bit;
            } else {
                return Err(token.error("undeclared modifier"));
            }
        }
        Ok(mask)
    }

    fn parse_type(&self, tokens: &[Token<'_>]) -> Result<Type> {
        let (header, body) = syntax::block(tokens)?;
        let name = match header {
            [kind, name] if kind.is("type") => *name,
            _ => return Err(syntax::error("expected named type")),
        };
        let mut typ = Type {
            name: name.string()?,
            offset: name.offset,
            modifiers: Mask::default(),
            entries: BTreeMap::new(),
            levels: 1,
            unsupported: None,
        };
        let mut has_modifiers = false;
        let mut level_names = BTreeMap::new();
        for statement in syntax::statements(body)? {
            let (left, right) = syntax::assignment(statement)?;
            let field = left
                .first()
                .ok_or_else(|| typ.error("missing type field"))?;
            if field.is("modifiers") && left.len() == 1 {
                if has_modifiers {
                    return Err(typ.error("duplicate modifiers field"));
                }
                typ.modifiers = self.expression(right)?;
                has_modifiers = true;
            } else if field.is("map") || field.is("preserve") {
                let modifiers = self.expression(syntax::index(left)?)?;
                let order = typ.entries.len();
                let entry = typ.entries.entry(modifiers).or_insert_with(|| Entry {
                    order,
                    ..Entry::default()
                });
                if field.is("map") {
                    let level = level(right)?;
                    if entry.level.replace(level).is_some() {
                        return Err(typ.error("duplicate map entry"));
                    }
                    typ.levels = typ.levels.max(level + 1);
                } else if entry.preserve.replace(self.expression(right)?).is_some() {
                    return Err(typ.error("duplicate preserve entry"));
                }
            } else if field.is("level_name") {
                let level = level(syntax::index(left)?)?;
                let label = match right {
                    [label] => label.string()?,
                    _ => return Err(typ.error("invalid level name")),
                };
                if level_names.insert(level, label).is_some() {
                    return Err(typ.error("duplicate level name"));
                }
                typ.levels = typ.levels.max(level + 1);
            } else {
                typ.unsupported.get_or_insert_with(|| Diagnostic {
                    offset: field.offset,
                    item: format!("{}.{}", typ.name, field.text),
                    reason: "unsupported type field",
                });
            }
        }
        for (mask, entry) in &typ.entries {
            if !mask.subset(typ.modifiers)
                || !entry.preserve.unwrap_or_default().subset(typ.modifiers)
            {
                return Err(typ.error("entry uses modifiers outside the type mask"));
            }
        }
        Ok(typ)
    }
}

fn named_header(header: &[Token<'_>], name: &str) -> Result<()> {
    match header {
        [kind] if kind.is(name) => Ok(()),
        [kind, label] if kind.is(name) => label.string().map(|_| ()),
        _ => Err(header.first().map_or_else(
            || syntax::error("missing block header"),
            |t| t.error("unsupported block header"),
        )),
    }
}

fn terms<'a, 's>(tokens: &'a [Token<'s>]) -> Result<impl Iterator<Item = &'a Token<'s>>> {
    if tokens.is_empty() || tokens.len().is_multiple_of(2) {
        return Err(syntax::error("expected modifier sum"));
    }
    for (index, token) in tokens.iter().enumerate() {
        if (index % 2 == 0 && token.kind != Kind::Word) || (index % 2 == 1 && !token.is("+")) {
            return Err(token.error("expected modifier sum"));
        }
    }
    Ok(tokens.iter().step_by(2))
}

fn real_expression(tokens: &[Token<'_>]) -> Result<u32> {
    let mut mask = 0;
    for token in terms(tokens)? {
        mask |= real_modifier(token.text)
            .or_else(|| number(token.text))
            .ok_or_else(|| token.error("virtual encoding must use real modifiers"))?;
    }
    Ok(mask)
}

// These are XKB's named real modifiers, not physical shortcut assignments.
fn real_modifier(name: &str) -> Option<u32> {
    [
        "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
    ]
    .iter()
    .position(|real| name.eq_ignore_ascii_case(real))
    .map(|bit| 1 << bit)
    .or_else(|| {
        if name.eq_ignore_ascii_case("none") {
            Some(0)
        } else if name.eq_ignore_ascii_case("all") {
            Some(0xff)
        } else {
            None
        }
    })
}

fn number(text: &str) -> Option<u32> {
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        text.parse().ok()
    }
}

fn level(tokens: &[Token<'_>]) -> Result<usize> {
    let token = match tokens {
        [token] if token.kind == Kind::Word => token,
        _ => return Err(syntax::error("expected level number")),
    };
    let number = if token
        .text
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Level"))
    {
        // Named levels have a decimal suffix; Level0x2 is not Level2.
        token
            .text
            .get(5..)
            .and_then(|suffix| suffix.parse::<u32>().ok())
    } else {
        number(token.text)
    };
    let level = number
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (1..=MAX_LEVELS).contains(n))
        .ok_or_else(|| token.error("level must be 1 through 16"))?;
    Ok(level - 1)
}
