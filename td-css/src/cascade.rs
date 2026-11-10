//! Ordered origin/importance cascade over immutable compiled stylesheets.
use crate::conditions::{self, Environment};
use crate::selectors::{Element, MatchBudget, SelectorList, Specificity};
use crate::syntax;
use crate::values::{
    self, ComputedStyle, Property, Specified, ValueDeclaration, MAX_ABSOLUTE_VALUE,
};
use crate::variables::{self, CustomDeclaration, CustomSpecified};
use crate::{Error, MAX_DECLARATIONS};
use std::collections::BTreeMap;

mod index;
pub use index::MAX_INDEX_ENTRIES;

pub const MAX_COMPILED_RULES: usize = 16_384;
pub const MAX_GROUP_DEPTH: usize = 16;
pub const MAX_GROUP_RULES: usize = 32_768;

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
    custom: Vec<CustomDeclaration>,
    origin: Origin,
}

#[derive(Clone, Debug, Default)]
pub struct StyleSet {
    rules: Vec<Rule>,
    index: index::RuleIndex,
}

/// Per-call counters, including partial work when computation is refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComputeStats {
    pub candidate_rules: usize,
    pub matched_rules: usize,
    pub match_steps: usize,
    pub substitution_components: usize,
    pub substitution_bytes: usize,
}

struct Work {
    budget: MatchBudget,
    substitution: variables::Budget,
    stats: ComputeStats,
    #[cfg(test)]
    unindexed: bool,
}

impl Default for Work {
    fn default() -> Self {
        Self {
            budget: MatchBudget::new(),
            substitution: variables::Budget::default(),
            stats: ComputeStats::default(),
            #[cfg(test)]
            unindexed: false,
        }
    }
}

impl StyleSet {
    /// Sheet order is source order. Supply HTML_UA_CSS explicitly if wanted.
    pub fn new(sheets: &[(Origin, &str)]) -> Result<Self, Error> {
        Self::new_with_environment(sheets, &Environment::default())
    }

    pub fn new_with_environment(
        sheets: &[(Origin, &str)],
        environment: &Environment,
    ) -> Result<Self, Error> {
        environment.validate()?;
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
        let mut compiler = Compiler {
            output: Self::default(),
            declarations: 0,
            walked: 0,
            environment,
        };
        for (origin, source) in sheets {
            compiler.add(
                syntax::parse_stylesheet(source)?.into_inner(),
                *origin,
                0,
                true,
            )?;
        }
        Ok(compiler.output)
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
        self.compute_with_stats(
            element,
            parent,
            root_font_size,
            inline,
            &mut ComputeStats::default(),
        )
    }

    /// Same semantics and bounds as compute; resets counters even on failure.
    pub fn compute_with_stats<E: Element>(
        &self,
        element: &E,
        parent: Option<&ComputedStyle>,
        root_font_size: f64,
        inline: &str,
        stats: &mut ComputeStats,
    ) -> Result<ComputedStyle, Error> {
        let mut work = Work::default();
        let result = self.compute_inner(element, parent, root_font_size, inline, &mut work);
        work.stats.match_steps = work.budget.used();
        work.stats.substitution_components = work.substitution.components;
        work.stats.substitution_bytes = work.substitution.bytes;
        *stats = work.stats;
        result
    }

    fn compute_inner<E: Element>(
        &self,
        element: &E,
        parent: Option<&ComputedStyle>,
        root_font_size: f64,
        inline: &str,
        work: &mut Work,
    ) -> Result<ComputedStyle, Error> {
        if !root_font_size.is_finite() || !(0.0..=MAX_ABSOLUTE_VALUE).contains(&root_font_size) {
            return Err(Error::InvalidContext);
        }
        let mut winners = BTreeMap::new();
        let mut custom_winners = BTreeMap::new();
        #[cfg(not(test))]
        let candidates = self.index.candidates(element, &mut work.budget)?;
        #[cfg(test)]
        let candidates = if work.unindexed {
            (0..self.rules.len()).collect()
        } else {
            self.index.candidates(element, &mut work.budget)?
        };
        work.stats.candidate_rules = candidates.len();
        let mut order = 0usize;
        for index in candidates {
            let Some(rule) = self.rules.get(index) else {
                continue;
            };
            let specificity = rule.selectors.matches_with(element, &mut work.budget)?;
            if let Some(specificity) = specificity {
                work.stats.matched_rules += 1;
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
                for declaration in &rule.custom {
                    choose_custom(
                        &mut custom_winners,
                        declaration,
                        Priority {
                            rank: rule.origin.rank(declaration.important),
                            inline: false,
                            specificity,
                            order,
                        },
                    )?;
                    order += 1;
                }
            }
        }
        let inline = syntax::parse_declarations(inline)?;
        let mut count = 0usize;
        for declaration in inline.value() {
            if let Some(custom) = variables::parse(declaration) {
                if count == MAX_DECLARATIONS {
                    return Err(Error::Limit("MAX_DECLARATIONS"));
                }
                count += 1;
                choose_custom(
                    &mut custom_winners,
                    &custom,
                    Priority {
                        rank: Origin::Author.rank(custom.important),
                        inline: true,
                        specificity: Specificity::default(),
                        order,
                    },
                )?;
                order += 1;
                continue;
            }
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
        let custom = variables::compute(
            custom_winners
                .into_iter()
                .map(|(name, winner)| (name, winner.value))
                .collect(),
            parent.map(|p| &p.custom),
            &mut work.substitution,
        )?;
        let mut style = ComputedStyle {
            custom,
            ..ComputedStyle::default()
        };
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
            let specified = values::resolve_pending(
                property,
                specified,
                &style.custom,
                &mut work.substitution,
            )?;
            let value = values::compute_value(
                property,
                &specified,
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
            )?;
            style.values.insert(property, value);
        }
        values::finish(&mut style, parent.is_none());
        Ok(style)
    }
}

struct Compiler<'a> {
    output: StyleSet,
    declarations: usize,
    walked: usize,
    environment: &'a Environment,
}
impl Compiler<'_> {
    fn add(
        &mut self,
        rules: Vec<syntax::Rule>,
        origin: Origin,
        depth: usize,
        active: bool,
    ) -> Result<(), Error> {
        for rule in rules {
            if self.walked == MAX_GROUP_RULES {
                return Err(Error::Limit("MAX_GROUP_RULES"));
            }
            self.walked += 1;
            if let Some(name) = &rule.name {
                let condition = if name.eq_ignore_ascii_case("media") {
                    conditions::media(&rule.prelude, self.environment)?
                } else if name.eq_ignore_ascii_case("supports") {
                    conditions::supports(&rule.prelude)?
                } else {
                    continue;
                };
                if let Some(block) = rule.block {
                    if depth == MAX_GROUP_DEPTH {
                        return Err(Error::Limit("MAX_GROUP_DEPTH"));
                    }
                    self.add(
                        syntax::rule_list(block, 0, false)?.into_inner(),
                        origin,
                        depth + 1,
                        active && condition,
                    )?;
                }
                continue;
            }
            if !active {
                continue;
            }
            let selectors = match SelectorList::from_components(&rule.prelude) {
                Ok(s) => s,
                Err(Error::InvalidSelector) => continue,
                Err(e) => return Err(e),
            };
            let mut declarations = Vec::new();
            let mut custom = Vec::new();
            for declaration in rule.declarations()?.value() {
                if let Some(value) = variables::parse(declaration) {
                    if self.declarations == MAX_DECLARATIONS {
                        return Err(Error::Limit("MAX_DECLARATIONS"));
                    }
                    self.declarations += 1;
                    custom.push(value);
                    continue;
                }
                for value in values::parse_declaration(declaration)? {
                    if self.declarations == MAX_DECLARATIONS {
                        return Err(Error::Limit("MAX_DECLARATIONS"));
                    }
                    self.declarations += 1;
                    declarations.push(value);
                }
            }
            if self.output.rules.len() == MAX_COMPILED_RULES {
                return Err(Error::Limit("MAX_COMPILED_RULES"));
            }
            self.output
                .index
                .insert(self.output.rules.len(), &selectors)?;
            self.output.rules.push(Rule {
                selectors,
                declarations,
                custom,
                origin,
            });
        }
        Ok(())
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

struct CustomWinner {
    priority: Priority,
    value: CustomSpecified,
}
fn choose_custom(
    winners: &mut BTreeMap<String, CustomWinner>,
    declaration: &CustomDeclaration,
    priority: Priority,
) -> Result<(), Error> {
    let old = winners.get(&declaration.name);
    if old.is_none() && winners.len() == variables::MAX_CUSTOM_PROPERTIES {
        return Err(Error::Limit("MAX_CUSTOM_PROPERTIES"));
    }
    if old.is_none_or(|winner| priority >= winner.priority) {
        winners.insert(
            declaration.name.clone(),
            CustomWinner {
                priority,
                value: declaration.value.clone(),
            },
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests;
