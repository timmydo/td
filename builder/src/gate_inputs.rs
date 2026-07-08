//! gate_inputs.rs — resolution for a GateDef's TYPED artifact inputs (#353).
//!
//! A gate declares the store-path artifacts it consumes (`GateDef::inputs`);
//! the runner resolves each declaration here BEFORE the script body runs and
//! exports the resolved path as `TD_GATE_INPUT_<NAME>` — the typed replacement
//! for the per-gate shell wiring (`grep -- '-<stem>-' LOCK | head -1`,
//! `store-closure-scan … | grep … | head -1`) that hid the gate's real
//! dependency graph inside its body. An input that fails to resolve (missing
//! lock, no match, AMBIGUOUS match — where `head -1` silently picked one) reds
//! the gate without running its body.
//!
//! Matching is exact-package, not substring: `bash` matches `…-bash-5.2.37`
//! and never `…-bash-static-5.2.37` (the shell needed `grep -v static` for
//! that), by requiring the character after the stem to start a version
//! (a digit). Uniqueness is REQUIRED — two matches are an error, never a
//! silent first-wins.

use crate::gates::{ArtifactInput, InputKind};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// The env var carrying a resolved input: `TD_GATE_INPUT_<NAME>` with the
/// declared name upper-cased and `-` mapped to `_` (names are validated as
/// `valid_word` at load time, so this is total).
pub(crate) fn env_var(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| match c {
            '-' | '.' | '+' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect();
    format!("TD_GATE_INPUT_{mapped}")
}

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

/// The UNIQUE candidate naming `stem` — zero or several is an error (the
/// shell's `head -1` picked one silently; a typed input refuses to guess).
fn unique_match<'a, I: Iterator<Item = &'a str>>(
    candidates: I,
    stem: &str,
    what: &str,
) -> Result<String, String> {
    let mut hits: Vec<&str> = candidates.filter(|p| path_names_stem(p, stem)).collect();
    hits.sort_unstable();
    hits.dedup();
    match hits.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("no entry names `{stem}` in {what}")),
        many => Err(format!(
            "`{stem}` is ambiguous in {what} ({} matches: {})",
            many.len(),
            many.join(", ")
        )),
    }
}

/// Resolve the UNIQUE lock entry naming `stem` to its store PATH.
fn resolve_lock_entry(root: &Path, lock: &str, stem: &str) -> Result<String, String> {
    let lock_path = root.join(lock);
    let text = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("read {}: {e}", lock_path.display()))?;
    // Lock lines are `NAME PATH [CLASS]` (crate::lock); the source key is only
    // needed to CLASSIFY untyped lines, which resolution doesn't use — matching
    // is on the PATH, so parse with a never-matching source name.
    let entries = crate::lock::parse(&text, "")?;
    unique_match(entries.iter().map(|e| e.path.as_str()), stem, lock)
}

/// Per-process memo for ClosureMember resolutions. Many gates declare the
/// IDENTICAL triple (many gates share the same bash-static fixture) and every
/// resolution pays a full store-dir index + a byte-scan of the root's whole
/// runtime closure, so one gate-run process resolves each distinct triple ONCE.
/// Keyed by the ABSOLUTE lock path (unit tests run many roots in one process).
/// Only Ok is memoized: a failure may be transient ordering (the subject gets
/// realized by build-recipes after an early non-build gate asked), and caching
/// it would poison every later gate in the run. Computed under the lock on
/// purpose — single-flight, so concurrent workers asking for the same triple
/// wait for the one scan instead of racing their own.
type ClosureKey = (String, String, String);
fn closure_memo() -> &'static Mutex<HashMap<ClosureKey, String>> {
    static MEMO: OnceLock<Mutex<HashMap<ClosureKey, String>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The uncached ClosureMember walk: content-scan the runtime closure of the
/// lock entry naming `root_stem` and pick the unique member naming
/// `member_stem`. NOTE this runs in the RUNNER process, outside the per-gate
/// prlimit/cgroup budgets that bound gate bodies — the memo above keeps it to
/// one scan per triple per run, which is the same order of work the old
/// per-gate `store-closure-scan` subprocess did once.
fn resolve_closure_member(
    root: &Path,
    lock: &str,
    root_stem: &str,
    member_stem: &str,
) -> Result<String, String> {
    let root_path = resolve_lock_entry(root, lock, root_stem)?;
    // The scanned store dir is the root entry's OWN prefix dir — no
    // hardcoded /gnu/store, so a /td/store lock resolves the same way.
    let store_dir = match root_path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir.to_string(),
        _ => return Err(format!("lock entry `{root_stem}` ({root_path}) is not a store path")),
    };
    let dirs = vec![store_dir.clone()];
    let (candidates, on_disk) = crate::scan_candidate_index(&dirs, &store_dir)?;
    let mut scanner = crate::scan::Scanner::new(&candidates).map_err(|e| e.to_string())?;
    let empty = std::collections::HashMap::new();
    let closure = crate::scan_closure_hybrid(
        &mut scanner,
        &on_disk,
        &empty,
        std::slice::from_ref(&root_path),
    )?;
    unique_match(
        closure.iter().map(String::as_str),
        member_stem,
        &format!("the content-scanned closure of {root_path}"),
    )
}

/// Resolve one declared input to a store path. `root` is the repo root (lock
/// paths are repo-relative, as everywhere else in the gate ladder).
pub(crate) fn resolve(root: &Path, input: &ArtifactInput) -> Result<String, String> {
    match &input.kind {
        InputKind::LockEntry { lock, stem } => resolve_lock_entry(root, lock, stem),
        InputKind::ClosureMember { lock, root_stem, member_stem } => {
            let key = (
                root.join(lock).display().to_string(),
                (*root_stem).to_string(),
                (*member_stem).to_string(),
            );
            let mut memo =
                closure_memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(hit) = memo.get(&key) {
                return Ok(hit.clone());
            }
            let resolved = resolve_closure_member(root, lock, root_stem, member_stem)?;
            memo.insert(key, resolved.clone());
            Ok(resolved)
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates::{ArtifactInput, InputKind};

    const H1: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const H2: &str = "0123456789abcdfghijklmnpqrsvwxyz";
    const H3: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir()
            .join(format!("td-gate-inputs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn leak(s: String) -> &'static str {
        Box::leak(s.into_boxed_str())
    }

    #[test]
    fn env_var_maps_the_declared_name() {
        assert_eq!(env_var("coreutils"), "TD_GATE_INPUT_COREUTILS");
        assert_eq!(env_var("bash-static"), "TD_GATE_INPUT_BASH_STATIC");
    }

    #[test]
    fn stem_matching_is_exact_package_not_substring() {
        let p = |b: &str| format!("/gnu/store/{H1}-{b}");
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

    #[test]
    fn lock_entry_resolves_unique_and_refuses_ambiguity() {
        let d = tmpdir("lock");
        std::fs::write(
            d.join("a.lock"),
            format!(
                "# comment\n\
                 {H1}-coreutils-9.1 /gnu/store/{H1}-coreutils-9.1\n\
                 {H2}-bash-5.2.37 /gnu/store/{H2}-bash-5.2.37\n\
                 {H3}-bash-static-5.2.37 /gnu/store/{H3}-bash-static-5.2.37\n"
            ),
        )
        .unwrap();
        let got = resolve_lock_entry(&d, "a.lock", "coreutils").unwrap();
        assert_eq!(got, format!("/gnu/store/{H1}-coreutils-9.1"));
        // `bash` skips bash-static without any grep -v.
        let got = resolve_lock_entry(&d, "a.lock", "bash").unwrap();
        assert_eq!(got, format!("/gnu/store/{H2}-bash-5.2.37"));
        // no match and a missing lock are loud errors.
        let e = resolve_lock_entry(&d, "a.lock", "gawk").unwrap_err();
        assert!(e.contains("no entry names `gawk`"), "got: {e}");
        assert!(resolve_lock_entry(&d, "missing.lock", "bash").is_err());
        // ambiguity is an error, never a silent head -1.
        std::fs::write(
            d.join("dup.lock"),
            format!(
                "{H1}-sed-4.9 /gnu/store/{H1}-sed-4.9\n\
                 {H2}-sed-4.8 /gnu/store/{H2}-sed-4.8\n"
            ),
        )
        .unwrap();
        let e = resolve_lock_entry(&d, "dup.lock", "sed").unwrap_err();
        assert!(e.contains("ambiguous"), "got: {e}");
    }

    /// A miniature on-disk store: root H1-app-1.0 references H2-dep-static-2.0
    /// by content (the 32-char hash appears in its bytes); H3-noise-3.0 is
    /// present but unreferenced. ClosureMember must find dep-static through the
    /// real content scan and must NOT match it under the stem `dep`.
    #[test]
    fn closure_member_resolves_through_the_content_scan() {
        let d = tmpdir("closure");
        let store = d.join("store");
        let app = format!("{H1}-app-1.0");
        let dep = format!("{H2}-dep-static-2.0");
        std::fs::create_dir_all(store.join(&app)).unwrap();
        std::fs::create_dir_all(store.join(&dep)).unwrap();
        std::fs::create_dir_all(store.join(format!("{H3}-noise-3.0"))).unwrap();
        // app's bytes carry dep's hash (how the daemon's scanForReferences links).
        std::fs::write(store.join(&app).join("bin"), format!("run {H2} now")).unwrap();
        std::fs::write(store.join(&dep).join("lib"), "static bytes").unwrap();
        std::fs::write(
            d.join("app.lock"),
            format!("{app} {}/{app}\n", store.display()),
        )
        .unwrap();

        let input = ArtifactInput {
            name: "dep-static",
            kind: InputKind::ClosureMember {
                lock: "app.lock",
                root_stem: "app",
                member_stem: "dep-static",
            },
        };
        let got = resolve(&d, &input).unwrap();
        assert_eq!(got, format!("{}/{dep}", store.display()));

        // `dep` is not `dep-static` — exact-package matching holds in closures too.
        let wrong = ArtifactInput {
            name: "dep",
            kind: InputKind::ClosureMember {
                lock: "app.lock",
                root_stem: "app",
                member_stem: "dep",
            },
        };
        let e = resolve(&d, &wrong).unwrap_err();
        assert!(e.contains("no entry names `dep`"), "got: {e}");

        // an unreferenced store entry is NOT in the closure (the scan is real,
        // not a directory listing).
        let noise = ArtifactInput {
            name: "noise",
            kind: InputKind::ClosureMember {
                lock: "app.lock",
                root_stem: "app",
                member_stem: "noise",
            },
        };
        assert!(resolve(&d, &noise).is_err());
    }

    #[test]
    fn closure_member_memoizes_ok_but_never_errors() {
        let d = tmpdir("memo");
        let store = d.join("store");
        let app = format!("{H1}-app-1.0");
        let dep = format!("{H2}-dep-static-2.0");
        std::fs::create_dir_all(store.join(&app)).unwrap();
        std::fs::create_dir_all(store.join(&dep)).unwrap();
        std::fs::write(store.join(&app).join("bin"), format!("run {H2} now")).unwrap();
        let input = ArtifactInput {
            name: "dep-static",
            kind: InputKind::ClosureMember {
                lock: "m.lock",
                root_stem: "app",
                member_stem: "dep-static",
            },
        };
        // An error (missing lock) is NOT memoized: fixing the cause fixes the
        // next resolution in the same process — a transient pre-build-recipes
        // failure must never poison a later gate's resolution.
        assert!(resolve(&d, &input).is_err());
        std::fs::write(d.join("m.lock"), format!("{app} {}/{app}\n", store.display())).unwrap();
        let got = resolve(&d, &input).unwrap();
        assert_eq!(got, format!("{}/{dep}", store.display()));
        // Ok IS memoized: delete the store; the same triple still resolves —
        // the second lookup answered from the memo, not a rescan.
        std::fs::remove_dir_all(&store).unwrap();
        assert_eq!(resolve(&d, &input).unwrap(), got);
    }

    #[test]
    fn resolve_dispatches_lock_entries_with_runtime_paths() {
        // The &'static declaration side is exercised end-to-end via leaked
        // runtime paths (what the scheduler tests do too).
        let d = tmpdir("dispatch");
        std::fs::write(
            d.join("l.lock"),
            format!("{H1}-make-4.4.1 /gnu/store/{H1}-make-4.4.1\n"),
        )
        .unwrap();
        let input = ArtifactInput {
            name: "make",
            kind: InputKind::LockEntry { lock: leak("l.lock".to_string()), stem: "make" },
        };
        assert_eq!(resolve(&d, &input).unwrap(), format!("/gnu/store/{H1}-make-4.4.1"));
    }
}
