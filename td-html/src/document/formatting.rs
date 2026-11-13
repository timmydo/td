use super::builder::Builder;
use super::*;
use crate::html5::{BYTE_CHARGE, MAX_ADOPTION_PASSES, MAX_FORMATTING_DUPLICATES};
impl Builder<'_> {
    pub(super) fn clone_element(&mut self, id: NodeId) -> Result<NodeId, ParseError> {
        let e = self.node(id)?.element().ok_or(ParseError::State)?;
        let bytes = e.attributes.iter().fold(e.name.len(), |n, a| {
            n.saturating_add(a.name.len()).saturating_add(a.value.len())
        });
        let count = e.attributes.len();
        self.tok.budget.charge(bytes.saturating_mul(BYTE_CHARGE))?;
        for _ in 0..count {
            self.step()?;
            self.tok.budget.attribute()?;
        }
        let e = self.node(id)?.element().ok_or(ParseError::State)?;
        let mut attributes =
            td_alloc::BudgetVec::with_capacity(&self.tok.budget.memory, e.attributes.len())?;
        for a in &e.attributes {
            attributes.try_push(Attribute {
                name: a.name.try_clone()?,
                value: a.value.try_clone()?,
                prefix: a.prefix,
                namespace: a.namespace,
            })?;
        }
        let e = Element {
            name: e.name.try_clone()?,
            namespace: e.namespace,
            attributes,
            template_contents: None,
            html_integration: e.html_integration,
        };
        self.allocate(NodeKind::Element(e))
    }
    pub(super) fn active_position(&mut self, id: NodeId) -> Result<Option<usize>, ParseError> {
        for i in (0..self.active.len()).rev() {
            self.step()?;
            if self.active.get(i) == Some(&Some(id)) {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }
    pub(super) fn active_named(
        &mut self,
        name: &str,
    ) -> Result<Option<(usize, NodeId)>, ParseError> {
        for i in (0..self.active.len()).rev() {
            self.step()?;
            let Some(id) = self.active.get(i).copied().flatten() else {
                break;
            };
            if self.is(id, name) {
                return Ok(Some((i, id)));
            }
        }
        Ok(None)
    }
    pub(super) fn add_active(&mut self, id: NodeId) -> Result<(), ParseError> {
        let mut count = 0;
        let mut earliest = None;
        for i in (0..self.active.len()).rev() {
            self.step()?;
            let Some(other) = self.active.get(i).copied().flatten() else {
                break;
            };
            let a = self.node(id)?.element().ok_or(ParseError::State)?;
            let b = self.node(other)?.element().ok_or(ParseError::State)?;
            let length = a.attributes.len();
            if a.name != b.name || a.namespace != b.namespace || length != b.attributes.len() {
                continue;
            }
            let mut equal = true;
            for ai in 0..length {
                let mut found = false;
                for bi in 0..length {
                    self.step()?;
                    let a = self
                        .node(id)?
                        .element()
                        .and_then(|e| e.attributes.get(ai))
                        .ok_or(ParseError::State)?;
                    let b = self
                        .node(other)?
                        .element()
                        .and_then(|e| e.attributes.get(bi))
                        .ok_or(ParseError::State)?;
                    if a == b {
                        found = true;
                        break;
                    }
                }
                if !found {
                    equal = false;
                    break;
                }
            }
            if equal {
                count += 1;
                earliest = Some(i);
            }
        }
        if count >= MAX_FORMATTING_DUPLICATES {
            if let Some(i) = earliest {
                self.active.remove(i);
            }
        }
        self.active.try_push(Some(id))?;
        Ok(())
    }
    pub(super) fn reconstruct(&mut self) -> Result<(), ParseError> {
        let mut start = self.active.len();
        while start > 0 {
            self.step()?;
            let Some(id) = self.active.get(start - 1).copied().flatten() else {
                break;
            };
            if self.open_position(id)?.is_some() {
                break;
            }
            start -= 1;
        }
        for i in start..self.active.len() {
            self.step()?;
            let old = self
                .active
                .get(i)
                .copied()
                .flatten()
                .ok_or(ParseError::State)?;
            let id = self.clone_element(old)?;
            let (p, b) = self.location(None)?;
            self.attach(id, p, b)?;
            self.push_open(id)?;
            *self.active.get_mut(i).ok_or(ParseError::State)? = Some(id);
        }
        Ok(())
    }
    pub(super) fn any_end(&mut self, name: &str) -> Result<(), ParseError> {
        for i in (0..self.open.len()).rev() {
            self.step()?;
            let id = *self.open.get(i).ok_or(ParseError::State)?;
            if self.is(id, name) {
                self.implied(name, false)?;
                self.open.truncate(i);
                return Ok(());
            }
            if self
                .node(id)?
                .element()
                .is_some_and(super::foreign::special)
            {
                self.error();
                return Ok(());
            }
        }
        Ok(())
    }
    pub(super) fn adoption(&mut self, name: &str) -> Result<(), ParseError> {
        if self.current_is(name) && self.active_position(self.current())?.is_none() {
            self.open.pop();
            return Ok(());
        }
        for _ in 0..MAX_ADOPTION_PASSES {
            self.step()?;
            let Some((format_index, format)) = self.active_named(name)? else {
                return self.any_end(name);
            };
            let Some(open_index) = self.open_position(format)? else {
                self.error();
                self.active.remove(format_index);
                return Ok(());
            };
            if !self.scoped(name, "")? {
                self.error();
                return Ok(());
            }
            let mut furthest = None;
            for i in open_index + 1..self.open.len() {
                self.step()?;
                let id = *self.open.get(i).ok_or(ParseError::State)?;
                if self
                    .node(id)?
                    .element()
                    .is_some_and(super::foreign::special)
                {
                    furthest = Some((i, id));
                    break;
                }
            }
            let Some((mut index, furthest)) = furthest else {
                self.open.truncate(open_index);
                self.active.remove(format_index);
                return Ok(());
            };
            let ancestor = *self
                .open
                .get(open_index.checked_sub(1).ok_or(ParseError::State)?)
                .ok_or(ParseError::State)?;
            let mut bookmark = format_index;
            let mut last = furthest;
            let mut inner = 0;
            loop {
                self.step()?;
                inner += 1;
                index = index.checked_sub(1).ok_or(ParseError::State)?;
                let node = *self.open.get(index).ok_or(ParseError::State)?;
                if node == format {
                    break;
                }
                if inner > 3 {
                    if let Some(i) = self.active_position(node)? {
                        self.active.remove(i);
                        if i < bookmark {
                            bookmark -= 1;
                        }
                    }
                }
                let Some(active) = self.active_position(node)? else {
                    self.open.remove(index);
                    continue;
                };
                let copy = self.clone_element(node)?;
                *self.active.get_mut(active).ok_or(ParseError::State)? = Some(copy);
                *self.open.get_mut(index).ok_or(ParseError::State)? = copy;
                if last == furthest {
                    bookmark = active + 1;
                }
                self.attach(last, copy, None)?;
                last = copy;
            }
            let (p, b) = self.location(Some(ancestor))?;
            self.attach(last, p, b)?;
            let replacement = self.clone_element(format)?;
            while let Some(child) = self.node(furthest)?.first {
                self.step()?;
                self.attach(child, replacement, None)?;
            }
            self.attach(replacement, furthest, None)?;
            if let Some(i) = self.active_position(format)? {
                self.active.remove(i);
                if i < bookmark {
                    bookmark -= 1;
                }
            }
            if bookmark > self.active.len() {
                return Err(ParseError::State);
            }
            self.active.try_push(Some(replacement))?;
            self.active
                .get_mut(bookmark..)
                .ok_or(ParseError::State)?
                .rotate_right(1);
            if let Some(i) = self.open_position(format)? {
                self.open.remove(i);
            }
            let at = self.open_position(furthest)?.ok_or(ParseError::State)? + 1;
            if self.open.len() >= self.tok.budget.limits.depth {
                return Err(ParseError::Depth);
            }
            self.open.try_push(replacement)?;
            self.open
                .get_mut(at..)
                .ok_or(ParseError::State)?
                .rotate_right(1);
        }
        Ok(())
    }
}
