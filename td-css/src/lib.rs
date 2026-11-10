//! CSS without a tree, layout, I/O or external dependencies. See DESIGN.md.
//!
//! The caller owns the tree and computes parents before their children:
//!
//! ```no_run
//! use td_css::{Origin, StyleSet, HTML_UA_CSS};
//! use td_css::selectors::Element;
//! use td_css::values::{ComputedValue, Length, Property};
//!
//! #[derive(Clone)]
//! struct Paragraph;
//! impl Element for Paragraph {
//!     fn tag_name(&self) -> &str { "p" }
//!     fn attribute(&self, _: &str) -> Option<&str> { None }
//!     fn parent(&self) -> Option<Self> { None }
//!     fn previous_sibling(&self) -> Option<Self> { None }
//!     fn next_sibling(&self) -> Option<Self> { None }
//!     fn first_child(&self) -> Option<Self> { None }
//!     fn has_text(&self) -> bool { true }
//!     fn is_link(&self) -> bool { false }
//! }
//!
//! let sheets = StyleSet::new(&[
//!     (Origin::UserAgent, HTML_UA_CSS),
//!     (Origin::Author, "p { margin: 2em; font-size: 20px }")
//! ])?;
//! let style = sheets.compute(&Paragraph, None, 16.0, "color: #123")?;
//! assert_eq!(style.font_size(), 20.0);
//! assert_eq!(style.get(Property::MarginTop),
//!            Some(&ComputedValue::Length(Length::Px(40.0))));
//! # Ok::<(), td_css::Error>(())
//! ```
#![forbid(unsafe_code)]

pub mod cascade;
pub mod conditions;
pub mod selectors;
pub mod syntax;
pub mod values;

pub use cascade::{ComputeStats, Origin, StyleSet, HTML_UA_CSS};
pub use values::{ComputedStyle, ComputedValue, Property};

#[cfg(test)]
mod test_tree;

pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
pub const MAX_TOKENS: usize = 131_072;
pub const MAX_NESTING: usize = 32;
pub const MAX_RULES: usize = 8192;
pub const MAX_DECLARATIONS: usize = 32_768;

/// Resource refusal is distinct from recoverable CSS parse errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Limit(&'static str),
    InvalidSelector,
    InvalidContext,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit(name) => write!(f, "CSS limit exceeded: {name}"),
            Self::InvalidSelector => f.write_str("unsupported or invalid CSS selector"),
            Self::InvalidContext => f.write_str("invalid CSS computation context"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod confinement {
    #[test]
    fn standalone_without_dependencies() {
        let manifest = include_str!("../Cargo.toml");
        assert!(!manifest.contains("[dependencies]"));
        assert!(!manifest.contains("[dev-dependencies]"));
        assert_eq!(
            include_str!("../Cargo.lock").matches("[[package]]").count(),
            1
        );
    }
}
