//! Validated, deterministic keyboard translation. No display, descriptors,
//! environment, action execution or wall clock is accessed here.

use crate::xkb::{self, Diagnostic, ResolvedType, Result, TypeCatalog, VirtualBinding};
use crate::xkb_compat::Compatibility;
use crate::xkb_keys::{self, Key};
use crate::xkb_symbols::{self as symbols, Role, Symbol};
use crate::xkb_syntax::{self as syntax, Kind};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Modifiers {
    pub depressed: u32,
    pub latched: u32,
    pub locked: u32,
    pub group: u32,
}

impl Modifiers {
    fn effective(self) -> u32 {
        self.depressed | self.latched | self.locked
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stroke {
    pub chord: String,
    pub repeat: bool,
}

/// Event-local refusal, not a failed keymap compilation. Adapters diagnose
/// the event without invalidating the previously compiled keyboard map.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputError {
    UnsupportedState(Diagnostic),
    UnsupportedSymbol(Diagnostic),
}

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedState(error) | Self::UnsupportedSymbol(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for InputError {}

/// Inspection data for tests and adapters. The keysym remains named when it
/// lies outside the small numeric vocabulary; it is never guessed by code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selected<'a> {
    pub name: &'a str,
    pub keysym: Option<u32>,
    pub level: usize,
    pub consumed: u32,
    pub repeat: bool,
}

#[derive(Debug)]
struct CompiledKey {
    source: Key,
    typ: Option<ResolvedType>,
    repeat: bool,
}

#[derive(Debug)]
pub struct Keymap {
    keys: BTreeMap<u32, CompiledKey>,
    virtuals: BTreeMap<String, u32>,
    roles: BTreeMap<Role, u32>,
    allowed: u32,
}

impl Keymap {
    /// Compile the entire supplied map before producing any editor input.
    /// This does not adopt a Wayland keyboard or start repeat scheduling.
    pub fn parse(source: &str) -> Result<Self> {
        let tokens = syntax::lex(source)?;
        for (position, token) in tokens.iter().enumerate() {
            if token.kind == Kind::Word
                && token.is("RedirectKey")
                && tokens.get(position + 1).is_some_and(|t| t.is("("))
            {
                return Err(token.error("redirect actions are unsupported"));
            }
        }
        let sections = syntax::sections(&tokens)?;
        let types = TypeCatalog::from_sections(&sections)?;
        let section = |name| {
            sections
                .get(name)
                .copied()
                .ok_or_else(|| syntax::error("missing section"))
        };
        let keys = xkb_keys::parse(section("xkb_keycodes")?, section("xkb_symbols")?, &types)?;
        let compatibility = Compatibility::parse(section("xkb_compatibility")?)?;
        let mut virtuals: BTreeMap<String, u32> = types
            .virtuals()
            .map(|(name, explicit)| (name.to_owned(), explicit))
            .collect();
        // Validate declarations even when no current key matches an interpret.
        for settings in compatibility.settings() {
            if let Some(name) = &settings.virtual_mod {
                require_virtual(&virtuals, name)?;
            }
            if let Some(modifiers) = &settings.action_mods {
                modifier_mask(modifiers, 0, &virtuals)?;
            }
        }
        for key in keys.values() {
            if let Some(names) = &key.virtuals {
                for name in names {
                    if name.eq_ignore_ascii_case("none") {
                        continue;
                    }
                    let value = virtuals
                        .get_mut(name)
                        .ok_or_else(|| key.error("undeclared virtual modifier"))?;
                    *value |= key.modifier;
                }
            } else {
                for (level, symbol) in key.symbols.iter().enumerate() {
                    if let Some(name) = compatibility
                        .matching(symbol, key.modifier, level)
                        .filter(|settings| !settings.first_only || level == 0)
                        .and_then(|settings| settings.virtual_mod.as_ref())
                    {
                        let value = virtuals
                            .get_mut(name)
                            .ok_or_else(|| key.error("undeclared virtual modifier"))?;
                        *value |= key.modifier;
                    }
                }
            }
        }
        let mut roles = BTreeMap::new();
        let mut compiled = BTreeMap::new();
        let bindings: Vec<_> = virtuals
            .iter()
            .map(|(name, mask)| VirtualBinding { name, mask: *mask })
            .collect();
        for (code, key) in keys {
            let used = key.used();
            if used {
                if let Some(error) = &key.unsupported {
                    return Err(with_key(error.clone(), &key));
                }
            }
            let mut repeat = key.repeat.unwrap_or_else(|| {
                key.symbols
                    .first()
                    .is_some_and(|symbol| symbol.value != Some(0))
            });
            for (level, symbol) in key.symbols.iter().enumerate() {
                let settings = compatibility.matching(symbol, key.modifier, level);
                if level == 0 && key.repeat.is_none() {
                    if let Some(settings) = settings {
                        repeat = settings.repeat;
                    }
                }
                if used {
                    if let Some(error) = settings.and_then(|settings| settings.unsupported.as_ref())
                    {
                        return Err(with_key(error.clone(), &key));
                    }
                }
                if let Some(role) = symbol.value.and_then(symbols::role) {
                    if settings.is_some_and(|settings| settings.role_unsupported) {
                        return Err(key.error("unsupported shortcut modifier action"));
                    }
                    let mask = match settings.and_then(|settings| settings.action_mods.as_ref()) {
                        Some(names) => modifier_mask(names, key.modifier, &virtuals)?,
                        None => key.modifier,
                    };
                    *roles.entry(role).or_insert(0u32) |= mask;
                }
            }
            let resolved = key
                .inferred_type()
                .and_then(|name| types.resolve(name, &bindings));
            let typ = if used {
                Some(resolved.map_err(|error| with_key(error, &key))?)
            } else {
                resolved.ok()
            };
            compiled.insert(
                code,
                CompiledKey {
                    source: key,
                    typ,
                    repeat,
                },
            );
        }
        let mut allowed = 0;
        for mask in roles.values() {
            if allowed & mask != 0 {
                return Err(syntax::error("ambiguous shortcut modifier masks"));
            }
            allowed |= mask;
        }
        let map = Self {
            keys: compiled,
            virtuals,
            roles,
            allowed,
        };
        // Exhaust all combinations of the handled real bits. High explicit
        // encodings are refused for shortcut roles to keep admission bounded.
        if map.allowed > 0xff {
            return Err(syntax::error("shortcut roles require real modifier bits"));
        }
        for key in map.keys.values().filter(|key| key.source.used()) {
            for state in 0..=255u32 {
                if state & !map.allowed != 0 {
                    continue;
                }
                if let Some((symbol, _)) = select(key, state) {
                    if !symbol.supported() && !symbol.ignored() {
                        return Err(symbol.error(
                            &key.source.name,
                            "unsupported keysym reachable in the ASCII profile",
                        ));
                    }
                }
            }
        }
        Ok(map)
    }

    pub fn virtual_mask(&self, name: &str) -> Option<u32> {
        self.virtuals.get(name).copied()
    }
    pub(crate) fn pointer_extend(&self, modifiers: Modifiers) -> bool {
        self.state(modifiers)
            .is_ok_and(|state| state & self.role(Role::Shift) != 0)
    }
    /// Declared Wayland/evdev key numbers with symbols, including out-of-profile keys.
    pub fn keycodes(&self) -> impl Iterator<Item = u32> + '_ {
        self.keys.keys().filter_map(|code| code.checked_sub(8))
    }

    /// Uses the Wayland/evdev key number (XKB keycode minus eight).
    /// Unknown codes are ignored, including values whose +8 would overflow.
    pub fn lookup(
        &self,
        evdev: u32,
        modifiers: Modifiers,
    ) -> std::result::Result<Option<Selected<'_>>, InputError> {
        let Some(key) = evdev.checked_add(8).and_then(|code| self.keys.get(&code)) else {
            return Ok(None);
        };
        let state = self
            .state(modifiers)
            .map_err(InputError::UnsupportedState)?;
        let Some((symbol, selection)) = select(key, state) else {
            return Ok(None);
        };
        let mut keysym = symbol.value;
        // XKB's unconsumed Lock transformation applies after type selection.
        if state & 2 != 0 && selection.consumed & 2 == 0 {
            if let Some(value) = keysym.filter(|value| (97..=122).contains(value)) {
                keysym = Some(value - 32);
            }
        }
        Ok(Some(Selected {
            name: &symbol.name,
            keysym,
            level: selection.level,
            consumed: selection.consumed,
            repeat: key.repeat,
        }))
    }

    pub fn translate(
        &self,
        evdev: u32,
        modifiers: Modifiers,
    ) -> std::result::Result<Option<Stroke>, InputError> {
        let Some(selected) = self.lookup(evdev, modifiers)? else {
            return Ok(None);
        };
        let state = modifiers.effective();
        let unconsumed = state & !selected.consumed;
        let control = unconsumed & self.role(Role::Control) != 0;
        let alt = unconsumed & self.role(Role::Alt) != 0;
        let mut shift = unconsumed & self.role(Role::Shift) != 0;
        let value = selected.keysym;
        if value.and_then(symbols::role).is_some() {
            return Ok(None);
        }
        let base = if let Some(mut c) = value.and_then(symbols::ascii) {
            if control || alt {
                if c.is_ascii_alphabetic() {
                    c = c.to_ascii_lowercase();
                    shift = state & self.role(Role::Shift) != 0;
                }
                if c == ' ' {
                    "Space".to_owned()
                } else {
                    c.to_string()
                }
            } else {
                shift = false;
                c.to_string()
            }
        } else if let Some(command) = value.and_then(symbols::command) {
            shift |= value == Some(0xfe20);
            command.to_owned()
        } else {
            let Some(key) = evdev.checked_add(8).and_then(|code| self.keys.get(&code)) else {
                return Ok(None);
            };
            let Some((symbol, _)) = select(key, state) else {
                return Ok(None);
            };
            if symbol.ignored() {
                return Ok(None);
            }
            return Err(InputError::UnsupportedSymbol(
                symbol.error(&key.source.name, "unsupported keysym"),
            ));
        };
        let mut chord = String::with_capacity(16);
        if control {
            chord.push_str("C-");
        }
        if alt {
            chord.push_str("M-");
        }
        if shift {
            chord.push_str("S-");
        }
        chord.push_str(&base);
        Ok(Some(Stroke {
            chord,
            repeat: selected.repeat,
        }))
    }

    fn role(&self, role: Role) -> u32 {
        self.roles.get(&role).copied().unwrap_or(0)
    }
    fn state(&self, modifiers: Modifiers) -> Result<u32> {
        if modifiers.group != 0 {
            return Err(syntax::error("additional layout group is unsupported"));
        }
        let state = modifiers.effective();
        if state & !self.allowed != 0 {
            return Err(syntax::error(
                "unsupported modifier state (including AltGr/Super)",
            ));
        }
        Ok(state)
    }
}

fn select(key: &CompiledKey, state: u32) -> Option<(&Symbol, xkb::Selection)> {
    let selection = key.typ.as_ref()?.select(state);
    key.source
        .symbols
        .get(selection.level)
        .map(|symbol| (symbol, selection))
}

fn with_key(mut error: Diagnostic, key: &Key) -> Diagnostic {
    error.item = format!("<{}>:{}", key.name, error.item);
    error
}

fn require_virtual(virtuals: &BTreeMap<String, u32>, name: &str) -> Result<()> {
    if virtuals.contains_key(name) {
        Ok(())
    } else {
        Err(Diagnostic {
            offset: 0,
            item: name.to_owned(),
            reason: "undeclared virtual modifier",
        })
    }
}

fn modifier_mask(names: &[String], modmap: u32, virtuals: &BTreeMap<String, u32>) -> Result<u32> {
    let mut result = 0;
    for name in names {
        result |= if name.eq_ignore_ascii_case("modMapMods") {
            modmap
        } else if let Some(mask) = xkb::real_modifier(name)
            .or_else(|| xkb::number(name))
            .or_else(|| name.eq_ignore_ascii_case("all").then_some(0xff))
        {
            mask
        } else {
            *virtuals.get(name).ok_or_else(|| Diagnostic {
                offset: 0,
                item: name.clone(),
                reason: "undeclared action modifier",
            })?
        };
    }
    Ok(result)
}
