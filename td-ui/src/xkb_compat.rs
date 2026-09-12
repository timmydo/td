//! Compatibility interpretation matching. Actions are never executed; modifier
//! action operands are read only to identify logical shortcut modifier masks.

use crate::xkb::{self, Diagnostic, Result};
use crate::xkb_keys::{boolean, modifier_names};
use crate::xkb_symbols::Symbol;
use crate::xkb_syntax::{self as syntax, Token};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default)]
pub(crate) struct Settings {
    pub first_only: bool,
    pub repeat: bool,
    pub virtual_mod: Option<String>,
    pub action_mods: Option<Vec<String>>,
    pub role_unsupported: bool,
    pub unsupported: Option<Diagnostic>,
}

#[derive(Debug)]
struct Interpret {
    symbol: Option<Symbol>,
    predicate: u8,
    mask: u32,
    settings: Settings,
}

#[derive(Debug)]
pub(crate) struct Compatibility {
    interprets: Vec<Interpret>,
}

impl Compatibility {
    pub fn parse(tokens: &[Token<'_>]) -> Result<Self> {
        let mut defaults = Settings::default();
        let mut interprets = Vec::new();
        for statement in syntax::statements(tokens)? {
            let first = statement
                .first()
                .ok_or_else(|| syntax::error("empty compatibility statement"))?;
            if first.is("virtual_modifiers") {
                continue;
            }
            if first.is("indicator") {
                let (header, body) = syntax::block(statement)?;
                match header {
                    [_, name] => {
                        name.string()?;
                    }
                    _ => return Err(first.error("invalid indicator")),
                }
                for field in syntax::statements(body)? {
                    syntax::assignment(field)?;
                }
                continue;
            }
            if first.is("interpret") && statement.get(1).is_some_and(|t| t.is(".")) {
                property(
                    &mut defaults,
                    statement
                        .get(2..)
                        .ok_or_else(|| first.error("missing interpretation default"))?,
                )?;
                continue;
            }
            let (header, body) = syntax::block(statement)?;
            if !first.is("interpret") {
                return Err(first.error("unsupported compatibility statement"));
            }
            if interprets.len() == 1024 {
                return Err(first.error("more than 1024 interpretations"));
            }
            let tail = header
                .get(1..)
                .ok_or_else(|| first.error("missing interpretation"))?;
            let (symbol, predicate_tokens) = match tail {
                [] => (None, &[][..]),
                [plus, rest @ ..] if plus.is("+") => (None, rest),
                [symbol] => (Some(Symbol::parse(&[*symbol])?), &[][..]),
                [symbol, plus, rest @ ..] if plus.is("+") => {
                    (Some(Symbol::parse(&[*symbol])?), rest)
                }
                _ => return Err(first.error("unsupported interpretation header")),
            };
            let symbol = symbol.filter(|symbol| {
                !symbol.name.eq_ignore_ascii_case("Any") && symbol.value != Some(0)
            });
            let (predicate, mask) = predicate(predicate_tokens)?;
            if interprets.iter().any(|previous: &Interpret| {
                previous.predicate == predicate
                    && previous.mask == mask
                    && match (&previous.symbol, &symbol) {
                        (None, None) => true,
                        (Some(a), Some(b)) => a.matches(b),
                        _ => false,
                    }
            }) {
                return Err(first.error("duplicate interpretation header"));
            }
            let mut settings = defaults.clone();
            let mut fields = BTreeSet::new();
            for field in syntax::statements(body)? {
                let name = field
                    .first()
                    .ok_or_else(|| first.error("empty interpretation field"))?;
                if !fields.insert(name.text.to_ascii_lowercase()) {
                    return Err(name.error("duplicate interpretation field"));
                }
                property(&mut settings, field)?;
            }
            interprets.push(Interpret {
                symbol,
                predicate,
                mask,
                settings,
            });
        }
        Ok(Self { interprets })
    }

    pub fn matching(&self, symbol: &Symbol, modmap: u32, level: usize) -> Option<&Settings> {
        if symbol.value == Some(0) {
            return None;
        }
        let mut winner: Option<(&Settings, (bool, u8))> = None;
        for interpret in &self.interprets {
            if interpret
                .symbol
                .as_ref()
                .is_some_and(|expected| !symbol.matches(expected))
            {
                continue;
            }
            let modmap = if interpret.settings.first_only && level != 0 {
                0
            } else {
                modmap
            };
            let intersects = modmap & interpret.mask;
            let matches = match interpret.predicate {
                0 => modmap == 0 || intersects != 0,
                1 => intersects != 0,
                2 => intersects == 0,
                3 => intersects == interpret.mask,
                _ => modmap == interpret.mask,
            };
            let priority = (interpret.symbol.is_some(), interpret.predicate);
            if matches && winner.is_none_or(|(_, previous)| priority > previous) {
                winner = Some((&interpret.settings, priority));
            }
        }
        winner.map(|(settings, _)| settings)
    }

    pub fn settings(&self) -> impl Iterator<Item = &Settings> {
        self.interprets.iter().map(|i| &i.settings)
    }
}

fn predicate(tokens: &[Token<'_>]) -> Result<(u8, u32)> {
    if tokens.is_empty() {
        return Ok((0, 0xff));
    }
    if matches!(tokens, [token] if token.is("Any")) {
        return Ok((1, 0xff));
    }
    let operation = tokens
        .first()
        .ok_or_else(|| syntax::error("missing predicate"))?;
    if tokens.get(1).is_some_and(|t| t.is("(")) {
        let kind = ["AnyOfOrNone", "AnyOf", "NoneOf", "AllOf", "Exactly"]
            .iter()
            .position(|name| operation.is(name))
            .ok_or_else(|| operation.error("unsupported modifier predicate"))?;
        let mask = real_mask(syntax::group(
            tokens
                .get(1..)
                .ok_or_else(|| operation.error("missing predicate operands"))?,
            "(",
            ")",
        )?)?;
        Ok((kind as u8, mask))
    } else {
        Ok((4, real_mask(tokens)?))
    }
}

pub(crate) fn real_mask(tokens: &[Token<'_>]) -> Result<u32> {
    let mask = xkb::real_expression(tokens)?;
    if mask > 0xff {
        return Err(syntax::error("expected an eight-bit real modifier mask"));
    }
    Ok(mask)
}

fn property(settings: &mut Settings, tokens: &[Token<'_>]) -> Result<()> {
    let (left, right) = syntax::assignment(tokens)?;
    let field = match left {
        [field] => field,
        _ => return Err(syntax::error("unsupported interpretation field")),
    };
    if field.is("useModMapMods") {
        settings.first_only = match right {
            [value] if value.is("level1") => true,
            [value] if value.is("any") || value.is("anylevel") => false,
            _ => return Err(field.error("unsupported useModMapMods")),
        };
    } else if field.is("repeat") {
        settings.repeat = boolean(right)?;
    } else if field.is("virtualModifier") {
        let names = modifier_names(right)?;
        if names.len() != 1 {
            return Err(field.error("interpretation requires one virtual modifier"));
        }
        settings.virtual_mod = names.into_iter().next();
    } else if field.is("action") {
        let (modifiers, unsupported) = action(right)?;
        settings.action_mods = modifiers;
        settings.role_unsupported = unsupported;
    } else {
        settings
            .unsupported
            .get_or_insert_with(|| field.error("unsupported interpretation field"));
    }
    Ok(())
}

fn action(tokens: &[Token<'_>]) -> Result<(Option<Vec<String>>, bool)> {
    let first = tokens
        .first()
        .ok_or_else(|| syntax::error("missing action"))?;
    if first.is("{") {
        // Multi-action interpretation of a shortcut modifier is not supported.
        // Its other client-side metadata still has ordinary matching rules.
        syntax::group(tokens, "{", "}")?;
        return Ok((None, true));
    }
    let args = syntax::group(
        tokens
            .get(1..)
            .ok_or_else(|| first.error("missing action operands"))?,
        "(",
        ")",
    )?;
    if first.is("RedirectKey") {
        return Err(first.error("redirect actions are unsupported"));
    }
    if !["SetMods", "LatchMods", "LockMods"]
        .iter()
        .any(|name| first.is(name))
    {
        return Ok((None, !first.is("NoAction")));
    }
    let mut modifiers = None;
    for argument in syntax::split(args, ",") {
        if argument.first().is_some_and(|t| t.is("modifiers")) {
            let (left, right) = syntax::assignment(argument)?;
            if left.len() != 1 || modifiers.is_some() {
                return Err(first.error("invalid action modifier operands"));
            }
            modifiers = Some(modifier_names(right)?);
        }
    }
    // Without a modifiers operand these actions use modMapMods.
    Ok((
        Some(modifiers.unwrap_or_else(|| vec!["modMapMods".to_owned()])),
        false,
    ))
}
