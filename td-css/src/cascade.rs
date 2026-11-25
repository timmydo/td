//! Ordered origin/importance cascade over immutable compiled stylesheets.
use crate::conditions::{self, Environment};
use crate::selectors::{Element, MatchBudget, SelectorList, Specificity};
use crate::syntax;
use crate::values::{
    self, ComputedStyle, Property, Specified, ValueDeclaration, MAX_ABSOLUTE_VALUE,
};
use crate::variables::{self, CustomDeclaration};
use crate::{Error, MAX_DECLARATIONS};
use td_alloc::{Budget, BudgetMap, BudgetShared, BudgetVec};

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

/// One input to [`StyleSet::from_sheets`], in cascade order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Sheet<'a> {
    pub origin: Origin,
    pub source: &'a str,
    /// Media query lists that must all match: a `<style media>` or `<link
    /// media>` attribute, then each `@import` list on the way to the sheet.
    /// Empty applies the sheet everywhere, as absent attributes do.
    pub media: &'a [&'a str],
}
impl<'a> Sheet<'a> {
    pub const fn new(origin: Origin, source: &'a str) -> Self {
        Self {
            origin,
            source,
            media: &[],
        }
    }
    pub const fn with_media(self, media: &'a [&'a str]) -> Self {
        Self { media, ..self }
    }
    /// Text and media together, against the sheet's own input bound. Each
    /// list counts at least one byte, so their number is bounded too, and
    /// the sum stops once past the bound.
    fn bytes(&self) -> usize {
        let mut total = self.source.len();
        for list in self.media {
            if total > crate::MAX_INPUT_BYTES {
                break;
            }
            total = total.saturating_add(list.len().max(1));
        }
        total
    }
}

/// A sheet `from_sheets` left out: its index in the caller's list, its
/// origin and the bound it met alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub index: usize,
    pub origin: Origin,
    pub error: Error,
}

/// The sheets a compilation left out, in input order; at most MAX_SHEETS.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    list: [Option<Refusal>; MAX_SHEETS],
    len: usize,
}
impl Refused {
    const fn new() -> Self {
        Self {
            list: [None; MAX_SHEETS],
            len: 0,
        }
    }
    fn record(&mut self, refusal: Refusal) -> Result<(), Error> {
        *self.list.get_mut(self.len).ok_or(Error::InvalidContext)? = Some(refusal);
        self.len += 1;
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn iter(&self) -> impl Iterator<Item = &Refusal> {
        self.list.iter().flatten()
    }
}
impl std::fmt::Debug for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[derive(Debug)]
struct Rule {
    selectors: SelectorList,
    declarations: BudgetVec<ValueDeclaration>,
    custom: BudgetVec<CustomDeclaration>,
    origin: Origin,
}

#[derive(Debug)]
pub struct StyleSet {
    rules: BudgetVec<Rule>,
    index: index::RuleIndex,
    /// The initial rare group every style that sets none of its
    /// longhands shares.
    rare: BudgetShared<values::Rare>,
}

/// Per-call counters, including partial work when computation is refused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ComputeStats {
    pub candidate_rules: usize,
    pub matched_rules: usize,
    pub match_steps: usize,
    /// The part of match_steps that charged inspected string bytes.
    pub match_bytes: usize,
    pub substitution_components: usize,
    pub substitution_bytes: usize,
    /// Declarations of matched rules and inline style taken into the
    /// cascade, custom properties included.
    pub declarations: usize,
    /// The custom ones among declarations.
    pub custom_declarations: usize,
    /// Distinct custom names declared on the element itself, each of which
    /// its computation resolves.
    pub local_custom_properties: usize,
    /// Inherited custom properties copied into the element's changed map.
    pub custom_copies: usize,
}

struct Work {
    budget: MatchBudget,
    substitution: variables::Budget,
    stats: ComputeStats,
    #[cfg(test)]
    unindexed: bool,
}

impl Work {
    fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            budget: MatchBudget::new(),
            substitution: variables::Budget::new(memory)?,
            stats: ComputeStats::default(),
            #[cfg(test)]
            unindexed: false,
        })
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
        Self::new_with_environment_and_budget(sheets, environment, &crate::memory()?)
    }
    pub fn new_with_budget(sheets: &[(Origin, &str)], memory: &Budget) -> Result<Self, Error> {
        Self::new_with_environment_and_budget(sheets, &Environment::default(), memory)
    }
    /// The tuple forms stop at the first refusal, a sheet's or the set's.
    pub fn new_with_environment_and_budget(
        sheets: &[(Origin, &str)],
        environment: &Environment,
        memory: &Budget,
    ) -> Result<Self, Error> {
        let at = |index: usize| {
            sheets
                .get(index)
                .map(|&(origin, source)| Sheet::new(origin, source))
        };
        Self::compile(sheets.len(), &at, environment, memory, false).map(|(set, _)| set)
    }

    /// Sheets with media conditions. A sheet a per-sheet bound refuses is
    /// left out, as if never given, and listed in Refused; a set-wide
    /// refusal is the Err.
    pub fn from_sheets(
        sheets: &[Sheet<'_>],
        environment: &Environment,
        memory: &Budget,
    ) -> Result<(Self, Refused), Error> {
        Self::compile(
            sheets.len(),
            &|index| sheets.get(index).copied(),
            environment,
            memory,
            true,
        )
    }

    /// Capacity a rollback can leave unused in the set's own arrays.
    #[cfg(test)]
    pub(crate) fn spare_bytes(&self) -> usize {
        (self.rules.capacity() - self.rules.len()) * std::mem::size_of::<Rule>()
            + self.index.spare_bytes()
    }

    /// One body for both slice forms; `at` yields each of `count` sheets.
    fn compile<'a>(
        count: usize,
        at: &dyn Fn(usize) -> Option<Sheet<'a>>,
        environment: &Environment,
        memory: &Budget,
        leave_out: bool,
    ) -> Result<(Self, Refused), Error> {
        environment.validate()?;
        if count > MAX_SHEETS {
            return Err(Error::Limit("MAX_SHEETS"));
        }
        // A sheet past its own bound is refused below, not counted here.
        let mut bytes = 0usize;
        for sheet in (0..count).filter_map(at) {
            let own = sheet.bytes();
            if own <= crate::MAX_INPUT_BYTES {
                bytes = bytes.saturating_add(own);
            }
        }
        if bytes > MAX_STYLE_BYTES {
            return Err(Error::Limit("MAX_STYLE_BYTES"));
        }
        let mut compiler = Compiler::new(environment, memory)?;
        let mut refused = Refused::new();
        for index in 0..count {
            let Some(sheet) = at(index) else {
                continue;
            };
            let mark = compiler.mark();
            match compiler.sheet(sheet, memory) {
                Ok(()) => {}
                // Only a limit within one sheet is its own; memory is the set's.
                Err(error @ Error::Limit(_)) if leave_out => {
                    // A total the sheets before it filled is the set's,
                    // unless the sheet alone meets a bound of its own.
                    let total = compiler.total;
                    compiler.rollback(mark);
                    let own = if total {
                        Compiler::new(environment, memory)?.alone(sheet, memory)?
                    } else {
                        Some(error)
                    };
                    let Some(own) = own else {
                        return Err(error);
                    };
                    refused.record(Refusal {
                        index,
                        origin: sheet.origin,
                        error: own,
                    })?;
                }
                Err(error) => return Err(error),
            }
        }
        compiler.output.index.finish();
        Ok((compiler.output, refused))
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
        self.compute_with_stats_and_budget(
            element,
            parent,
            root_font_size,
            inline,
            stats,
            self.rules.budget(),
        )
    }
    pub fn compute_with_budget<E: Element>(
        &self,
        element: &E,
        parent: Option<&ComputedStyle>,
        root_font_size: f64,
        inline: &str,
        memory: &Budget,
    ) -> Result<ComputedStyle, Error> {
        self.compute_with_stats_and_budget(
            element,
            parent,
            root_font_size,
            inline,
            &mut ComputeStats::default(),
            memory,
        )
    }
    pub fn compute_with_stats_and_budget<E: Element>(
        &self,
        element: &E,
        parent: Option<&ComputedStyle>,
        root_font_size: f64,
        inline: &str,
        stats: &mut ComputeStats,
        memory: &Budget,
    ) -> Result<ComputedStyle, Error> {
        let mut work = Work::new(memory)?;
        let result = self.compute_inner(element, parent, root_font_size, inline, &mut work);
        work.stats.match_steps = work.budget.used();
        work.stats.match_bytes = work.budget.bytes();
        work.stats.substitution_components = work.substitution.components;
        work.stats.substitution_bytes = work.substitution.bytes;
        work.stats.custom_copies = work.substitution.copies;
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
        let memory = work.substitution.memory.try_clone()?;
        let mut winners = BudgetMap::new(&memory)?;
        // Custom declarations are sorted once rather than inserted one by
        // one: a map's shifting insertion is quadratic in descending names.
        let mut inline_custom = td_alloc::BudgetVec::new(&memory)?;
        let mut custom_candidates = td_alloc::BudgetVec::new(&memory)?;
        #[cfg(not(test))]
        let candidates = self.index.candidates(element, &mut work.budget, &memory)?;
        #[cfg(test)]
        let candidates = if work.unindexed {
            {
                let mut all = td_alloc::BudgetVec::with_capacity(&memory, self.rules.len())?;
                for index in 0..self.rules.len() {
                    all.try_push(index)?;
                }
                all
            }
        } else {
            self.index.candidates(element, &mut work.budget, &memory)?
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
                work.stats.declarations = work
                    .stats
                    .declarations
                    .saturating_add(rule.declarations.len())
                    .saturating_add(rule.custom.len());
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
                    )?;
                    order += 1;
                }
                for declaration in &rule.custom {
                    custom_candidates.try_push((
                        declaration,
                        Priority {
                            rank: rule.origin.rank(declaration.important),
                            inline: false,
                            specificity,
                            order,
                        },
                    ))?;
                    order += 1;
                }
            }
        }
        let inline = syntax::parse_declarations_with_budget(inline, &memory)?;
        let mut count = 0usize;
        for declaration in inline.value() {
            if let Some(custom) = variables::parse(declaration, &memory)? {
                if count == MAX_DECLARATIONS {
                    return Err(Error::Limit("MAX_DECLARATIONS"));
                }
                count += 1;
                work.stats.declarations = work.stats.declarations.saturating_add(1);
                let priority = Priority {
                    rank: Origin::Author.rank(custom.important),
                    inline: true,
                    specificity: Specificity::default(),
                    order,
                };
                inline_custom.try_push((custom, priority))?;
                order += 1;
                continue;
            }
            for value in values::parse_declaration(declaration, &memory)? {
                if count == MAX_DECLARATIONS {
                    return Err(Error::Limit("MAX_DECLARATIONS"));
                }
                count += 1;
                work.stats.declarations = work.stats.declarations.saturating_add(1);
                choose(
                    &mut winners,
                    &value,
                    Priority {
                        rank: Origin::Author.rank(value.important),
                        inline: true,
                        specificity: Specificity::default(),
                        order,
                    },
                )?;
                order += 1;
            }
        }
        for (declaration, priority) in inline_custom.iter() {
            custom_candidates.try_push((declaration, *priority))?;
        }
        work.stats.custom_declarations = custom_candidates.len();
        custom_candidates.sort_unstable_by(|(a, x), (b, y)| a.name.cmp(&b.name).then(x.cmp(y)));
        // Each name's last candidate has the highest priority and wins.
        let mut custom_local = BudgetMap::new(&memory)?;
        let mut winners_of = custom_candidates.iter().peekable();
        while let Some((declaration, _)) = winners_of.next() {
            if winners_of
                .peek()
                .is_some_and(|(next, _)| next.name == declaration.name)
            {
                continue;
            }
            if custom_local.len() == variables::MAX_CUSTOM_PROPERTIES {
                return Err(Error::Limit("MAX_CUSTOM_PROPERTIES"));
            }
            custom_local.try_insert(
                declaration.name.try_clone()?,
                declaration.value.try_clone()?,
            )?;
        }
        work.stats.local_custom_properties = custom_local.len();
        let custom = variables::compute(
            custom_local,
            parent.map(|p| &p.custom),
            &mut work.substitution,
        )?;
        let common = Property::common_roster();
        let mut style = ComputedStyle {
            custom,
            values: td_alloc::BudgetMap::with_capacity(&memory, common.len())?,
            rare: self.rare.try_clone()?,
        };
        // Dependency order: font size, then color, then all remaining
        // common values; the rare ones, which none depends on, after.
        let properties = [Property::FontSize, Property::Color].into_iter().chain(
            common
                .iter()
                .copied()
                .filter(|p| !matches!(p, Property::FontSize | Property::Color)),
        );
        // The style's own font size and color, once computed: what
        // style.font_size() and style.color() would answer, without a
        // search of the incomplete map for every property. Before them
        // they hold those accessors' defaults: font-size, computed first,
        // takes no color, and color, second, resolves `currentcolor` from
        // its parent; both match the accessors on a map lacking them.
        let (mut font_size, mut color) = (16.0, values::Color::BLACK);
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
                font_size,
                color,
                if parent.is_none() {
                    font_size
                } else {
                    root_font_size
                },
                &memory,
            )?;
            match (property, &value) {
                (Property::FontSize, values::ComputedValue::Length(values::Length::Px(px))) => {
                    font_size = *px;
                }
                (Property::Color, values::ComputedValue::Color(c)) => color = *c,
                _ => {}
            }
            style.values.try_insert(property, value)?;
        }
        // Only the rare longhands a declaration won, which sort last; the
        // shared initial group stands for the rest, and is copied once,
        // when a value first differs from it.
        let root_font_size = if parent.is_none() {
            font_size
        } else {
            root_font_size
        };
        let mut rare: Option<values::Rare> = None;
        for (property, winner) in winners.iter().rev() {
            let Some(slot) = property.rare() else {
                break;
            };
            let specified = values::resolve_pending(
                *property,
                &winner.value,
                &style.custom,
                &mut work.substitution,
            )?;
            let value = values::compute_value(
                *property,
                &specified,
                parent,
                font_size,
                color,
                root_font_size,
                &memory,
            )?;
            if rare.is_none() {
                if style.rare.values.get(slot).is_some_and(|v| v.same(&value)) {
                    continue;
                }
                rare = Some(style.rare.try_copy()?);
            }
            if let Some(target) = rare.as_mut().and_then(|g| g.values.get_mut(slot)) {
                *target = value;
            }
        }
        if let Some(rare) = rare {
            style.rare = BudgetShared::new(&memory, rare)?;
        }
        values::finish(&mut style, parent)?;
        Ok(style)
    }
}

#[derive(Clone, Copy)]
struct Mark {
    rules: usize,
    index: index::Mark,
    declarations: usize,
    walked: usize,
}

struct Compiler<'a> {
    output: StyleSet,
    declarations: usize,
    walked: usize,
    environment: &'a Environment,
    /// Set when a set-wide total, not the sheet, refused.
    total: bool,
}
impl<'a> Compiler<'a> {
    fn new(environment: &'a Environment, memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            output: StyleSet {
                rules: BudgetVec::new(memory)?,
                index: index::RuleIndex::new(memory)?,
                rare: BudgetShared::new(memory, values::Rare::initial()?)?,
            },
            declarations: 0,
            walked: 0,
            environment,
            total: false,
        })
    }

    fn exceeded(&mut self, total: &'static str) -> Error {
        self.total = true;
        Error::Limit(total)
    }

    fn mark(&self) -> Mark {
        Mark {
            rules: self.output.rules.len(),
            index: self.output.index.mark(),
            declarations: self.declarations,
            walked: self.walked,
        }
    }
    /// Forget a refused sheet: everything it added, nothing before it.
    fn rollback(&mut self, mark: Mark) {
        self.output.rules.truncate(mark.rules);
        self.output.index.rollback(mark.index);
        self.declarations = mark.declarations;
        self.walked = mark.walked;
        self.total = false;
    }

    /// The per-sheet bound `sheet` meets compiled alone, from empty totals;
    /// None when it compiles, or alone exceeds a set-wide total. Its storage
    /// is charged to the same budget and dropped on return.
    fn alone(mut self, sheet: Sheet<'_>, memory: &Budget) -> Result<Option<Error>, Error> {
        match self.sheet(sheet, memory) {
            Ok(()) => Ok(None),
            Err(error @ Error::Limit(_)) => Ok((!self.total).then_some(error)),
            Err(error) => Err(error),
        }
    }

    /// A sheet's media conditions act as enclosing @media groups would,
    /// without a group level, and cannot be closed by the sheet's text.
    /// Every list is evaluated, so its bounds hold whatever the others say.
    fn sheet(&mut self, sheet: Sheet<'_>, memory: &Budget) -> Result<(), Error> {
        if sheet.bytes() > crate::MAX_INPUT_BYTES {
            return Err(Error::Limit("MAX_INPUT_BYTES"));
        }
        let mut active = true;
        for list in sheet.media {
            let list = syntax::components(list, memory, syntax::Bom::Keep)?;
            active &= conditions::media(list.value(), self.environment, memory)?;
        }
        self.add(
            syntax::parse_stylesheet_with_budget(sheet.source, memory)?.into_inner(),
            sheet.origin,
            0,
            active,
        )
    }

    fn add(
        &mut self,
        rules: BudgetVec<syntax::Rule>,
        origin: Origin,
        depth: usize,
        active: bool,
    ) -> Result<(), Error> {
        let memory = self.output.rules.budget().try_clone()?;
        for rule in rules {
            if self.walked == MAX_GROUP_RULES {
                return Err(self.exceeded("MAX_GROUP_RULES"));
            }
            self.walked += 1;
            if let Some(name) = &rule.name {
                let condition = if name.eq_ignore_ascii_case("media") {
                    conditions::media(&rule.prelude, self.environment, &memory)?
                } else if name.eq_ignore_ascii_case("supports") {
                    conditions::supports(&rule.prelude, &memory)?
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
            let selectors = match SelectorList::from_components(&rule.prelude, &memory) {
                Ok(s) => s,
                Err(Error::InvalidSelector) => continue,
                Err(e) => return Err(e),
            };
            let mut declarations = BudgetVec::new(&memory)?;
            let mut custom = BudgetVec::new(&memory)?;
            for declaration in rule.declarations()?.value() {
                if let Some(value) = variables::parse(declaration, &memory)? {
                    if self.declarations == MAX_DECLARATIONS {
                        return Err(self.exceeded("MAX_DECLARATIONS"));
                    }
                    self.declarations += 1;
                    custom.try_push(value)?;
                    continue;
                }
                for value in values::parse_declaration(declaration, &memory)? {
                    if self.declarations == MAX_DECLARATIONS {
                        return Err(self.exceeded("MAX_DECLARATIONS"));
                    }
                    self.declarations += 1;
                    declarations.try_push(value)?;
                }
            }
            if self.output.rules.len() == MAX_COMPILED_RULES {
                return Err(self.exceeded("MAX_COMPILED_RULES"));
            }
            if let Err(error) = self
                .output
                .index
                .admit_rule(self.output.rules.len(), &selectors)
            {
                self.total = true;
                return Err(error);
            }
            self.output.rules.try_push(Rule {
                selectors,
                declarations,
                custom,
                origin,
            })?;
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
    winners: &mut BudgetMap<Property, Winner>,
    declaration: &ValueDeclaration,
    priority: Priority,
) -> Result<(), Error> {
    if winners
        .get(&declaration.property)
        .is_none_or(|winner| priority >= winner.priority)
    {
        winners.try_insert(
            declaration.property,
            Winner {
                priority,
                value: declaration.value.try_clone()?,
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod sheet_tests;
#[cfg(test)]
mod tests;
// Inline so its generated fixtures stay outside the scanned test file.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod winner_tests {
    use super::*;
    use crate::test_tree::{tree, Handle};
    #[test]
    fn custom_winners_are_sorted_once_and_attribute_scans_are_charged() {
        let nodes = tree();
        let element = Handle {
            nodes: &nodes,
            index: 3,
        };
        // Descending names, repeated: each name's highest-priority one wins.
        let css = "p{--c:red;--b:red;--a:red}p{--c:blue;--a:lime}p{--c:navy!important}";
        let set = StyleSet::new(&[(Origin::Author, css)]).unwrap();
        let mut stats = ComputeStats::default();
        let style = set
            .compute_with_stats(&element, None, 16.0, "--b:teal;--c:aqua", &mut stats)
            .unwrap();
        assert_eq!(stats.custom_declarations, 8);
        assert_eq!(stats.local_custom_properties, 3);
        for (name, value) in [("--a", "lime"), ("--b", "teal"), ("--c", "navy")] {
            let tokens = style.custom_property(name).unwrap();
            assert_eq!(
                syntax::trim(tokens).first().and_then(|c| c.ident()),
                Some(value)
            );
        }
        // A lookup pays for the attributes it compares, a step for every four.
        let steps = |attrs: Vec<(&'static str, String)>, css: &str| {
            let mut nodes = tree();
            nodes[3].attrs = attrs;
            let set = StyleSet::new(&[(Origin::Author, css)]).unwrap();
            let mut stats = ComputeStats::default();
            set.compute_with_stats(
                &Handle {
                    nodes: &nodes,
                    index: 3,
                },
                None,
                16.0,
                "",
                &mut stats,
            )
            .unwrap();
            stats.match_steps
        };
        let many: Vec<_> = (0..256)
            .map(|i| {
                (
                    &*Box::leak(format!("x{i:03}").into_boxed_str()),
                    String::new(),
                )
            })
            .collect();
        assert_eq!(
            steps(many, "[x255]{color:red}")
                - steps(vec![("x255", String::new())], "[x255]{color:red}"),
            64
        );
        // A case-folded substring search pays for its bytes again.
        let long = || vec![("t", "a".repeat(100))];
        assert_eq!(
            steps(long(), "[t*=zz i]{color:red}") - steps(long(), "[t*=zz]{color:red}"),
            25
        );
        // `:link` pays only where the caller compares attributes for it.
        let link = |tag: &'static str, attrs: Vec<(&'static str, String)>| {
            let mut nodes = tree();
            nodes[3].tag = tag;
            nodes[3].attrs = attrs;
            let set = StyleSet::new(&[(Origin::Author, ":link{color:red}")]).unwrap();
            let mut stats = ComputeStats::default();
            set.compute_with_stats(
                &Handle {
                    nodes: &nodes,
                    index: 3,
                },
                None,
                16.0,
                "",
                &mut stats,
            )
            .unwrap();
            stats.match_steps
        };
        let wide = || {
            (0..256)
                .map(|i| {
                    (
                        &*Box::leak(format!("y{i:03}").into_boxed_str()),
                        String::new(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(link("p", wide()), link("p", Vec::new()));
        assert_eq!(link("a", wide()) - link("a", Vec::new()), 64);
    }
}
