use crate::selectors::{checked, html_space, Element, IndexKey, MatchBudget, SelectorList};
use crate::Error;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_INDEX_ENTRIES: usize = 262_144;

#[derive(Clone, Debug, Default)]
pub(super) struct RuleIndex {
    ids: BTreeMap<String, Vec<usize>>,
    classes: BTreeMap<String, Vec<usize>>,
    tags: BTreeMap<String, Vec<usize>>,
    universal: Vec<usize>,
    entries: usize,
}

impl RuleIndex {
    pub(super) fn insert(&mut self, rule: usize, selectors: &SelectorList) -> Result<(), Error> {
        for key in selectors.index_keys() {
            if self.entries == MAX_INDEX_ENTRIES {
                return Err(Error::Limit("MAX_INDEX_ENTRIES"));
            }
            self.entries += 1;
            let bucket = match key {
                IndexKey::Id(name) => self.ids.entry(name).or_default(),
                IndexKey::Class(name) => self.classes.entry(name).or_default(),
                IndexKey::Tag(name) => self.tags.entry(name).or_default(),
                IndexKey::Universal => &mut self.universal,
            };
            bucket.push(rule);
        }
        Ok(())
    }

    pub(super) fn candidates<E: Element>(
        &self,
        element: &E,
        budget: &mut MatchBudget,
    ) -> Result<Vec<usize>, Error> {
        let mut output = BTreeSet::new();
        append(&mut output, &self.universal, budget)?;
        if !self.tags.is_empty() {
            let tag = checked(element.tag_name(), budget)?.to_ascii_lowercase();
            lookup(&mut output, &self.tags, &tag, budget)?;
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
        Ok(output.into_iter().collect())
    }
}

fn lookup(
    output: &mut BTreeSet<usize>,
    map: &BTreeMap<String, Vec<usize>>,
    key: &str,
    budget: &mut MatchBudget,
) -> Result<(), Error> {
    budget.charge(1)?;
    if let Some(bucket) = map.get(key) {
        append(output, bucket, budget)?;
    }
    Ok(())
}

fn append(
    output: &mut BTreeSet<usize>,
    bucket: &[usize],
    budget: &mut MatchBudget,
) -> Result<(), Error> {
    budget.charge(bucket.len())?;
    output.extend(bucket.iter().copied());
    Ok(())
}
