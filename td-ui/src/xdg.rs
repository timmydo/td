//! The XDG base directories a program keeps its files under.
//!
//! One rule, from the XDG Base Directory Specification: a variable counts
//! only when its value is an absolute path ("If an implementation
//! encounters a relative path in any of these variables it should consider
//! the path invalid and ignore it"), so an empty value is ignored too, and
//! the fallback under `HOME` counts only when `HOME` is absolute. With
//! neither there is no directory: never one relative to wherever the
//! program was started, nor a shared one such as `/tmp`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// One of the per-user base directories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// `XDG_CONFIG_HOME`, else `HOME/.config`.
    Config,
    /// `XDG_DATA_HOME`, else `HOME/.local/share`.
    Data,
    /// `XDG_STATE_HOME`, else `HOME/.local/state`.
    State,
    /// `XDG_CACHE_HOME`, else `HOME/.cache`.
    Cache,
}

impl Base {
    /// The environment variable that names this directory.
    pub fn variable(self) -> &'static str {
        match self {
            Base::Config => "XDG_CONFIG_HOME",
            Base::Data => "XDG_DATA_HOME",
            Base::State => "XDG_STATE_HOME",
            Base::Cache => "XDG_CACHE_HOME",
        }
    }

    /// The directory under `HOME` when the variable does not count.
    pub fn fallback(self) -> &'static str {
        match self {
            Base::Config => ".config",
            Base::Data => ".local/share",
            Base::State => ".local/state",
            Base::Cache => ".cache",
        }
    }

    /// Why there is no such directory, for a caller to report.
    pub fn missing(self) -> String {
        format!(
            "no {} directory: neither {} nor HOME is an absolute path",
            match self {
                Base::Config => "configuration",
                Base::Data => "data",
                Base::State => "state",
                Base::Cache => "cache",
            },
            self.variable()
        )
    }
}

/// The directory `base` names, given the values of its variable and of
/// `HOME`.
pub fn dir(base: Base, value: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    fn absolute(value: Option<&OsStr>) -> Option<&Path> {
        value.map(Path::new).filter(|path| path.is_absolute())
    }
    match absolute(value) {
        Some(path) => Some(path.to_path_buf()),
        None => Some(absolute(home)?.join(base.fallback())),
    }
}

/// The directory `base` names in this process's environment.
pub fn from_env(base: Base) -> Option<PathBuf> {
    dir(
        base,
        std::env::var_os(base.variable()).as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn an_absolute_variable_wins_and_home_is_the_fallback() {
        for base in [Base::Config, Base::Data, Base::State, Base::Cache] {
            assert_eq!(
                dir(base, os("/x/y"), os("/home/u")),
                Some(PathBuf::from("/x/y"))
            );
            assert_eq!(
                dir(base, None, os("/home/u")),
                Some(Path::new("/home/u").join(base.fallback()))
            );
        }
    }

    #[test]
    fn a_relative_or_empty_value_is_ignored_and_a_relative_home_is_none() {
        for value in ["", "relative", "./x", "~/x"] {
            assert_eq!(
                dir(Base::Cache, os(value), os("/home/u")),
                Some(PathBuf::from("/home/u/.cache")),
                "{value:?}"
            );
            assert_eq!(dir(Base::Cache, os(value), os(value)), None, "{value:?}");
        }
        assert_eq!(dir(Base::Config, None, None), None);
    }

    #[test]
    fn a_non_utf8_path_is_kept_as_given() {
        use std::os::unix::ffi::OsStrExt;
        let value = OsStr::from_bytes(b"/data/\xff");
        assert_eq!(
            dir(Base::Data, Some(value), None),
            Some(PathBuf::from(value))
        );
        let home = OsStr::from_bytes(b"/home/\xff");
        assert_eq!(
            dir(Base::Cache, None, Some(home)),
            Some(Path::new(home).join(".cache"))
        );
    }

    #[test]
    fn the_report_names_the_variables() {
        assert_eq!(
            Base::State.missing(),
            "no state directory: neither XDG_STATE_HOME nor HOME is an absolute path"
        );
    }
}
