//! gate_inputs.rs — `path_names_stem`: does a store path name a given package?
//!
//! Exact-package matching, not substring: `bash` matches `…-bash-5.2.37` and
//! never `…-bash-static-5.2.37`, by requiring the character after the stem to
//! start a version (a digit). Used by the substitution resolvers in main.rs
//! (TD_GCC_TOOLCHAIN / gcc-toolchain stem matching).
//!
//! (The #353 typed gate-input mechanism that also lived here — LockEntry /
//! ClosureMember resolution binding guix lock members into gate sandboxes — was
//! removed with the guix seed store: every gate now resolves its tools from the
//! td-built userland on PATH, so no gate declares a lock input.)

/// Does a store path name package `stem`? The basename after the 32-char
/// digest (the shape rule lives in `store::name_from_store_path` — one source,
/// not a fourth copy) must BE the stem or be `<stem>-<version…>` with the
/// version starting in a digit — so `bash` matches `bash-5.2.37`, not
/// `bash-static-5.2.37`. A non-store-shaped basename is matched as-is.
pub(crate) fn path_names_stem(path: &str, stem: &str) -> bool {
    let name_ver = match crate::store::name_from_store_path(path) {
        Some(n) => n,
        None => path.rsplit('/').next().unwrap_or(path).to_string(),
    };
    if name_ver == stem {
        return true;
    }
    name_ver
        .strip_prefix(stem)
        .and_then(|r| r.strip_prefix('-'))
        .and_then(|r| r.chars().next())
        .is_some_and(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn stem_matching_is_exact_package_not_substring() {
        let p = |b: &str| format!("/td/store/{H1}-{b}");
        // `bash` is bash, not bash-static / bash-minimal (the shell needed grep -v).
        assert!(path_names_stem(&p("bash-5.2.37"), "bash"));
        assert!(!path_names_stem(&p("bash-static-5.2.37"), "bash"));
        assert!(!path_names_stem(&p("bash-minimal-5.2.37"), "bash"));
        assert!(path_names_stem(&p("bash-static-5.2.37"), "bash-static"));
        // versionless exact name, and a name that is prefix of another package.
        assert!(path_names_stem(&p("bash"), "bash"));
        assert!(!path_names_stem(&p("bash"), "bas"));
        // a source tarball still names its package (version starts the suffix).
        assert!(path_names_stem(&p("fixture-1.0.tar.gz"), "fixture"));
    }
}
