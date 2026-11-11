//! Bounded HTML tokenization, independent of the resident renderer.
mod entities;
mod tokenizer;
pub use tokenizer::{Attribute, Doctype, Tag, TextMode, Token, Tokenizer};

pub const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_NODES: usize = 262_144;
pub const MAX_ATTRIBUTES: usize = 524_288;
pub const MAX_TREE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_DEPTH: usize = 256;
pub const MAX_ATTRIBUTES_PER_TAG: usize = 256;
pub const MAX_NAME_BYTES: usize = 1024;
pub const MAX_ATTRIBUTE_BYTES: usize = 1024 * 1024;
pub const MAX_WORK: usize = 64_000_000;
pub const MAX_ADOPTION_PASSES: usize = 8;
pub const MAX_FORMATTING_DUPLICATES: usize = 3;
pub const NODE_CHARGE: usize = 256;
pub const ATTRIBUTE_CHARGE: usize = 128;
pub const BYTE_CHARGE: usize = 2;

/// Caller ceilings, clamped to the constants above; zero refuses that resource.
#[derive(Clone, Copy, Debug)]
pub struct ParseLimits {
    pub input: usize,
    pub nodes: usize,
    pub attributes: usize,
    pub bytes: usize,
    pub depth: usize,
    pub attributes_per_tag: usize,
    pub name_bytes: usize,
    pub attribute_bytes: usize,
    pub work: usize,
}
impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            input: MAX_INPUT_BYTES,
            nodes: MAX_NODES,
            attributes: MAX_ATTRIBUTES,
            bytes: MAX_TREE_BYTES,
            depth: MAX_DEPTH,
            attributes_per_tag: MAX_ATTRIBUTES_PER_TAG,
            name_bytes: MAX_NAME_BYTES,
            attribute_bytes: MAX_ATTRIBUTE_BYTES,
            work: MAX_WORK,
        }
    }
}
impl ParseLimits {
    pub(crate) fn clamped(self) -> Self {
        Self {
            input: self.input.min(MAX_INPUT_BYTES),
            nodes: self.nodes.min(MAX_NODES),
            attributes: self.attributes.min(MAX_ATTRIBUTES),
            bytes: self.bytes.min(MAX_TREE_BYTES),
            depth: self.depth.min(MAX_DEPTH),
            attributes_per_tag: self.attributes_per_tag.min(MAX_ATTRIBUTES_PER_TAG),
            name_bytes: self.name_bytes.min(MAX_NAME_BYTES),
            attribute_bytes: self.attribute_bytes.min(MAX_ATTRIBUTE_BYTES),
            work: self.work.min(MAX_WORK),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    Input,
    Nodes,
    Attributes,
    Bytes,
    Depth,
    AttributesPerTag,
    NameBytes,
    AttributeBytes,
    Work,
    State,
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTML parse refused: {self:?}")
    }
}
impl std::error::Error for ParseError {}

pub(crate) struct Budget {
    pub limits: ParseLimits,
    pub bytes: usize,
    pub work: usize,
    pub attributes: usize,
}
impl Budget {
    pub fn new(limits: ParseLimits) -> Self {
        Self {
            limits: limits.clamped(),
            bytes: 0,
            work: 0,
            attributes: 0,
        }
    }
    pub fn charge(&mut self, bytes: usize) -> Result<(), ParseError> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .filter(|n| *n <= self.limits.bytes)
            .ok_or(ParseError::Bytes)?;
        Ok(())
    }
    pub fn step(&mut self) -> Result<(), ParseError> {
        self.work = self
            .work
            .checked_add(1)
            .filter(|n| *n <= self.limits.work)
            .ok_or(ParseError::Work)?;
        Ok(())
    }
    pub fn attribute(&mut self) -> Result<(), ParseError> {
        self.attributes = self
            .attributes
            .checked_add(1)
            .filter(|n| *n <= self.limits.attributes)
            .ok_or(ParseError::Attributes)?;
        self.charge(ATTRIBUTE_CHARGE)
    }
    pub fn push(
        &mut self,
        out: &mut String,
        c: char,
        limit: usize,
        error: ParseError,
    ) -> Result<(), ParseError> {
        if out.len().saturating_add(c.len_utf8()) > limit {
            return Err(error);
        }
        self.charge(c.len_utf8() * BYTE_CHARGE)?;
        out.push(c);
        Ok(())
    }
    pub fn append(&mut self, out: &mut String, text: &str) -> Result<(), ParseError> {
        self.charge(text.len().saturating_mul(BYTE_CHARGE))?;
        out.push_str(text);
        Ok(())
    }
}
#[cfg(test)]
mod tests;
