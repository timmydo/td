use crate::selectors::{checked, html_space, Element, IndexKey, MatchBudget, SelectorList};
use crate::syntax::Text;
use crate::Error;
use td_alloc::{Budget, BudgetVec};

pub const MAX_INDEX_ENTRIES: usize = 262_144;
#[derive(Debug)]
pub(super) struct RuleIndex {
    ids: BudgetVec<(Text, usize)>,
    classes: BudgetVec<(Text, usize)>,
    tags: BudgetVec<(Text, usize)>,
    universal: BudgetVec<usize>,
    entries: usize,
}
impl RuleIndex {
    pub(super) fn new(memory: &Budget) -> Result<Self, Error> {
        Ok(Self {
            ids: BudgetVec::new(memory)?,
            classes: BudgetVec::new(memory)?,
            tags: BudgetVec::new(memory)?,
            universal: BudgetVec::new(memory)?,
            entries: 0,
        })
    }
    pub(super) fn admit_rule(
        &mut self,
        rule: usize,
        selectors: &SelectorList,
    ) -> Result<(), Error> {
        for key in selectors.index_keys()? {
            if self.entries == MAX_INDEX_ENTRIES {
                return Err(Error::Limit("MAX_INDEX_ENTRIES"));
            }
            self.entries += 1;
            let (map, name) = match key {
                IndexKey::Id(name) => (&mut self.ids, name),
                IndexKey::Class(name) => (&mut self.classes, name),
                IndexKey::Tag(name) => (&mut self.tags, name),
                IndexKey::Universal => {
                    self.universal.try_push(rule)?;
                    continue;
                }
            };
            map.try_push((name.try_clone()?, rule))?;
        }
        Ok(())
    }
    pub(super) fn finish(&mut self) {
        for entries in [&mut self.ids, &mut self.classes, &mut self.tags] {
            entries.sort_unstable_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        }
    }
    pub(super) fn candidates<E: Element>(
        &self,
        element: &E,
        budget: &mut MatchBudget,
        memory: &Budget,
    ) -> Result<BudgetVec<usize>, Error> {
        let mut output = BudgetVec::new(memory)?;
        append(&mut output, &self.universal, budget)?;
        if !self.tags.is_empty() {
            let tag = checked(element.tag_name(), budget)?;
            if tag.bytes().any(|byte| byte.is_ascii_uppercase()) {
                let mut folded = td_alloc::BudgetString::copy(memory, tag)?;
                folded.make_ascii_lowercase();
                lookup(&mut output, &self.tags, &folded, budget)?;
            } else {
                lookup(&mut output, &self.tags, tag, budget)?;
            }
        }
        if !self.ids.is_empty() {
            if let Some(id) = element.attribute("id") {
                lookup(&mut output, &self.ids, checked(id, budget)?, budget)?;
            }
        }
        if !self.classes.is_empty() {
            if let Some(classes) = element.attribute("class") {
                for class in checked(classes, budget)?
                    .split(html_space)
                    .filter(|s| !s.is_empty())
                {
                    lookup(&mut output, &self.classes, class, budget)?;
                }
            }
        }
        output.sort_unstable();
        output.dedup();
        Ok(output)
    }
}
fn lookup(
    output: &mut BudgetVec<usize>,
    map: &BudgetVec<(Text, usize)>,
    key: &str,
    budget: &mut MatchBudget,
) -> Result<(), Error> {
    budget.charge(1)?;
    let start = map.partition_point(|(name, _)| name.as_str() < key);
    let tail = map.get(start..).ok_or(Error::Limit("MAX_INDEX_ENTRIES"))?;
    let count = tail.partition_point(|(name, _)| name.as_str() == key);
    budget.charge(count)?;
    BudgetVec::try_reserve(output, count)?;
    for (_, rule) in tail.iter().take(count) {
        output.try_push(*rule)?;
    }
    Ok(())
}
fn append(
    output: &mut BudgetVec<usize>,
    bucket: &[usize],
    budget: &mut MatchBudget,
) -> Result<(), Error> {
    budget.charge(bucket.len())?;
    output.try_extend_from_slice(bucket)?;
    Ok(())
}
