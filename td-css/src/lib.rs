//! CSS without a tree, layout, I/O or external dependencies. See DESIGN.md.
#![forbid(unsafe_code)]

pub mod selectors;
pub mod syntax;

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
