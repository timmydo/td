//! Ordered origin/importance cascade over immutable compiled stylesheets.
use crate::selectors::{Element, MatchBudget, SelectorList, Specificity};
use crate::syntax;
use crate::values::{
    self, ComputedStyle, Property, Specified, ValueDeclaration, MAX_ABSOLUTE_VALUE,
};
use crate::{Error, MAX_DECLARATIONS, MAX_RULES};
use std::collections::BTreeMap;

pub const MAX_SHEETS: usize = 64;
pub const MAX_STYLE_BYTES: usize = 4 * 1024 * 1024;
pub const HTML_UA_CSS: &str = include_str!("html.css");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    UserAgent,
    User,
    Author,
}

impl Origin {
    fn rank(self, important: bool) -> u8 {
        match (self, important) {
            (Self::UserAgent, false) => 0,
            (Self::User, false) => 1,
            (Self::Author, false) => 2,
            (Self::Author, true) => 3,
            (Self::User, true) => 4,
            (Self::UserAgent, true) => 5,
        }
    }
}

#[derive(Clone, Debug)]
struct Rule {
    selectors: SelectorList,
    declarations: Vec<ValueDeclaration>,
    origin: Origin,
}

#[derive(Clone, Debug, Default)]
pub struct StyleSet {
    rules: Vec<Rule>,
}

impl StyleSet {
    /// Sheet order is source order. Supply HTML_UA_CSS explicitly if wanted.
    pub fn new(sheets: &[(Origin, &str)]) -> Result<Self, Error> {
        if sheets.len() > MAX_SHEETS {
            return Err(Error::Limit("MAX_SHEETS"));
        }
        let mut bytes = 0usize;
        for (_, source) in sheets {
            bytes = bytes
                .checked_add(source.len())
                .ok_or(Error::Limit("MAX_STYLE_BYTES"))?;
            if bytes > MAX_STYLE_BYTES {
                return Err(Error::Limit("MAX_STYLE_BYTES"));
            }
        }
        let mut rules = Vec::new();
        let mut declarations_count = 0usize;
        for (origin, source) in sheets {
            let sheet = syntax::parse_stylesheet(source)?;
            for rule in sheet.value() {
                if rule.at_name().is_some() {
                    continue;
                }
                let selectors = match SelectorList::from_components(rule.prelude()) {
                    Ok(selectors) => selectors,
                    Err(Error::InvalidSelector) => continue,
                    Err(error) => return Err(error),
                };
                let mut declarations = Vec::new();
                for declaration in rule.declarations()?.value() {
                    for value in values::parse_declaration(declaration)? {
                        if declarations_count == MAX_DECLARATIONS {
                            return Err(Error::Limit("MAX_DECLARATIONS"));
                        }
                        declarations_count += 1;
                        declarations.push(value);
                    }
                }
                if rules.len() == MAX_RULES {
                    return Err(Error::Limit("MAX_RULES"));
                }
                rules.push(Rule {
                    selectors,
                    declarations,
                    origin: *origin,
                });
            }
        }
        Ok(Self { rules })
    }

    /// Parent styles must be computed first. None identifies the root, whose
    /// rem base is computed here. Descendants use the supplied root_font_size.
    pub fn compute<E: Element>(
        &self,
        element: &E,
        parent: Option<&ComputedStyle>,
        root_font_size: f64,
        inline: &str,
    ) -> Result<ComputedStyle, Error> {
        if !root_font_size.is_finite() || !(0.0..=MAX_ABSOLUTE_VALUE).contains(&root_font_size) {
            return Err(Error::InvalidContext);
        }
        let mut winners = BTreeMap::new();
        let mut budget = MatchBudget::new();
        let mut order = 0usize;
        for rule in &self.rules {
            let specificity = rule.selectors.matches_with(element, &mut budget)?;
            if let Some(specificity) = specificity {
                for declaration in &rule.declarations {
                    choose(
                        &mut winners,
                        declaration,
                        Priority {
                            rank: rule.origin.rank(declaration.important),
                            inline: false,
                            specificity,
                            order,
                        },
                    );
                    order += 1;
                }
            }
        }
        let inline = syntax::parse_declarations(inline)?;
        let mut count = 0usize;
        for declaration in inline.value() {
            for value in values::parse_declaration(declaration)? {
                if count == MAX_DECLARATIONS {
                    return Err(Error::Limit("MAX_DECLARATIONS"));
                }
                count += 1;
                choose(
                    &mut winners,
                    &value,
                    Priority {
                        rank: Origin::Author.rank(value.important),
                        inline: true,
                        specificity: Specificity::default(),
                        order,
                    },
                );
                order += 1;
            }
        }
        let mut style = ComputedStyle::default();
        // Dependency order: font size, then color, then all remaining values.
        let properties = [Property::FontSize, Property::Color].into_iter().chain(
            Property::ALL
                .iter()
                .copied()
                .filter(|p| !matches!(p, Property::FontSize | Property::Color)),
        );
        for property in properties {
            let specified = winners
                .get(&property)
                .map(|winner: &Winner| &winner.value)
                .unwrap_or(&Specified::Unset);
            let value = values::compute_value(
                property,
                specified,
                parent,
                style.font_size(),
                style.color(),
                if parent.is_none() {
                    if property == Property::FontSize {
                        16.0
                    } else {
                        style.font_size()
                    }
                } else {
                    root_font_size
                },
            );
            style.values.insert(property, value);
        }
        Ok(style)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Priority {
    rank: u8,
    inline: bool,
    specificity: Specificity,
    order: usize,
}

struct Winner {
    priority: Priority,
    value: Specified,
}

fn choose(
    winners: &mut BTreeMap<Property, Winner>,
    declaration: &ValueDeclaration,
    priority: Priority,
) {
    if winners
        .get(&declaration.property)
        .is_none_or(|winner| priority >= winner.priority)
    {
        winners.insert(
            declaration.property,
            Winner {
                priority,
                value: declaration.value.clone(),
            },
        );
    }
}

#[cfg(test)]
mod tests;
