//! Bounded streaming Boolean composition; condition parsing and matching are separate.

pub const MAX_DEPTH: usize = 64;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operator {
    And,
    Or,
    Not,
}
impl Operator {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "AND" => Some(Self::And),
            "OR" => Some(Self::Or),
            "NOT" => Some(Self::Not),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    DepthLimit,
    NodeLimit,
    Failed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "filter logic: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug)]
struct Frame {
    operator: Operator,
    value: bool,
}
const EMPTY: Frame = Frame {
    operator: Operator::Or,
    value: false,
};
/// Feed every parsed node in order, including children whose truth value cannot
/// change their parent's result. This helper never authorizes skipping validation.
#[derive(Debug)]
pub struct Evaluation {
    frames: [Frame; MAX_DEPTH],
    depth: usize,
    maximum_depth: usize,
    nodes: usize,
    maximum_nodes: usize,
    root: Option<bool>,
    failed: bool,
}
impl Evaluation {
    /// Bounds come from the admitted query. Depth counts each operator or
    /// condition as one logical level; JSON container depth is checked separately.
    pub fn new(maximum_depth: usize, maximum_nodes: usize) -> Result<Self, Error> {
        if maximum_depth == 0 || maximum_depth > MAX_DEPTH || maximum_nodes == 0 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            frames: [EMPTY; MAX_DEPTH],
            depth: 0,
            maximum_depth,
            nodes: 0,
            maximum_nodes,
            root: None,
            failed: false,
        })
    }
    fn enter(&mut self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        if self.root.is_some() {
            return Err(Error::Invalid);
        }
        if self.depth >= self.maximum_depth {
            return Err(Error::DepthLimit);
        }
        if self.nodes >= self.maximum_nodes {
            return Err(Error::NodeLimit);
        }
        self.nodes = self.nodes.checked_add(1).ok_or(Error::NodeLimit)?;
        Ok(())
    }
    pub fn begin(&mut self, operator: Operator) -> Result<(), Error> {
        self.enter()?;
        *self.frames.get_mut(self.depth).ok_or(Error::Invalid)? = Frame {
            operator,
            value: operator == Operator::And,
        };
        self.depth += 1;
        self.failed = false;
        Ok(())
    }
    pub fn condition(&mut self, value: bool) -> Result<(), Error> {
        self.enter()?;
        self.accept(value)?;
        self.failed = false;
        Ok(())
    }
    fn accept(&mut self, value: bool) -> Result<(), Error> {
        if self.depth == 0 {
            if self.root.replace(value).is_some() {
                return Err(Error::Invalid);
            }
        } else {
            let parent = self.frames.get_mut(self.depth - 1).ok_or(Error::Invalid)?;
            match parent.operator {
                Operator::And => parent.value &= value,
                Operator::Or | Operator::Not => parent.value |= value,
            }
        }
        Ok(())
    }
    pub fn end(&mut self) -> Result<(), Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        self.failed = true;
        self.depth = self.depth.checked_sub(1).ok_or(Error::Invalid)?;
        let frame = self.frames.get(self.depth).copied().ok_or(Error::Invalid)?;
        self.accept(if frame.operator == Operator::Not {
            !frame.value
        } else {
            frame.value
        })?;
        self.failed = false;
        Ok(())
    }
    /// Consume the evaluator only after the parser has admitted the whole filter.
    pub fn finish(self) -> Result<bool, Error> {
        if self.failed {
            return Err(Error::Failed);
        }
        if self.depth != 0 {
            return Err(Error::Invalid);
        }
        self.root.ok_or(Error::Invalid)
    }
}
#[cfg(test)]
#[path = "filter_logic_tests.rs"]
mod tests;
