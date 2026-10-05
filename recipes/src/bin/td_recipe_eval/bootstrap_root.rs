//! The pinned bootstrap root, evaluator side (AGENTS.md, "Target artifact
//! graph"; the builder's `bootstrap_root.rs` admits it).
//!
//! `seed/bootstrap-root.txt` pins the ladder's outputs at the gcc-14 cut. A
//! graph that is not itself part of the ladder stops at the root's exports
//! and stages them as audited seeds, so a builder ABI bump or a cold machine
//! no longer climbs from stage0 by default. The ladder still builds from
//! stage0 when a target is one of its rungs, which is how `bootstrap-root
//! check` and `bootstrap-root pin` reproduce it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use td_engine::bootstrap_root::{ladder_digest, Root};

use crate::catalog;
use crate::check_runner::{classify_graph_inputs, recipe_closure_full, SeedInput};
use crate::seed_digests;

const MANIFEST: &str = include_str!("../../../../seed/bootstrap-root.txt");

/// Opt out of the cut for one invocation: every graph climbs from stage0.
pub(crate) const FULL_ENV: &str = "TD_BOOTSTRAP_FROM_STAGE0";

/// The compiled manifest, parsed once.
pub(crate) fn root() -> Result<&'static Root, String> {
    static ROOT: OnceLock<Result<Root, String>> = OnceLock::new();
    ROOT.get_or_init(|| Root::parse(MANIFEST))
        .as_ref()
        .map_err(Clone::clone)
}

/// The manifest's own digest, which keys the local root db: two worktrees
/// pinning different roots never share one.
pub(crate) fn manifest_key() -> String {
    td_engine::sha256::hex_digest(MANIFEST.as_bytes())
}

/// The cut: the ladder outputs every post-cut recipe builds from. gcc-14
/// with its binutils and glibc, plus the mesboot-era build userland and
/// tools (bash, coreutils, make, m4, bison, python, ...) the native GNU
/// platform is configured with. A pinned root exports exactly these.
pub(crate) const CUT: &[&str] = &[
    "bash-mesboot",
    "binutils-244",
    "bison-mesboot",
    "coreutils-mesboot0",
    "diffutils-mesboot0",
    "gawk-mesboot",
    "gawk-mesboot0",
    "gcc-14",
    "glibc-mesboot",
    "glibc-mesboot-shared",
    "grep-mesboot0",
    "m4-mesboot",
    "make-441",
    "make-mesboot",
    "python-mesboot",
    "sed-mesboot0",
];

/// The ladder: every recipe the cut is built from, the cut included.
pub(crate) fn ladder() -> Result<&'static BTreeSet<String>, String> {
    static LADDER: OnceLock<Result<BTreeSet<String>, String>> = OnceLock::new();
    LADDER
        .get_or_init(|| {
            Ok(recipe_closure_full(CUT)?
                .into_iter()
                .map(|n| n.stem)
                .collect())
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// A recipe on the ladder's side of the cut: a rung, or one that builds
/// from a rung below the cut, directly or through another such recipe (a
/// bridge test, an orphan rung). It can only be built from stage0.
pub(crate) fn ladder_side(stem: &str) -> Result<bool, String> {
    Ok(ladder_side_set()?.contains(stem))
}

/// The ladder plus every recipe that reaches a rung below the cut without
/// passing through a cut export: a fixpoint over the catalog's edges.
fn ladder_side_set() -> Result<&'static BTreeSet<String>, String> {
    static SIDE: OnceLock<Result<BTreeSet<String>, String>> = OnceLock::new();
    SIDE.get_or_init(|| {
        let mut side = ladder()?.clone();
        let all = catalog::all();
        loop {
            let before = side.len();
            for (stem, recipe) in &all {
                if side.contains(*stem) {
                    continue;
                }
                if recipe
                    .inputs
                    .iter()
                    .chain(recipe.native_inputs.iter())
                    .chain(recipe.payload_inputs.iter())
                    .flatten()
                    .any(|i| side.contains(i) && !CUT.contains(&i.as_str()))
                {
                    side.insert(stem.to_string());
                }
            }
            if side.len() == before {
                return Ok(side);
            }
        }
    })
    .as_ref()
    .map_err(Clone::clone)
}

/// The export stems a walk over `targets` stops at, or `None` to walk the
/// whole chain: the root is unpinned, a target is a ladder rung, or the
/// operator asked for stage0 with `TD_BOOTSTRAP_FROM_STAGE0=1`.
pub(crate) fn cut_for(targets: &[&str]) -> Result<Option<BTreeSet<&'static str>>, String> {
    cut_for_with(
        targets,
        std::env::var_os(FULL_ENV).is_some_and(|v| v == "1"),
    )
}

/// `cut_for` with the operator's stage0 opt-out passed in, so a test does
/// not depend on the ambient environment.
pub(crate) fn cut_for_with(
    targets: &[&str],
    from_stage0: bool,
) -> Result<Option<BTreeSet<&'static str>>, String> {
    let root = root()?;
    if !root.is_pinned() || from_stage0 {
        return Ok(None);
    }
    for target in targets {
        if ladder_side(target)? {
            return Ok(None);
        }
    }
    Ok(Some(CUT.iter().copied().collect()))
}

/// What the ladder is built from NOW: each rung's canonical recipe JSON and
/// each seed it stages, by the basename the compiled digest table pins. The
/// manifest records this digest when it is pinned; any drift means the pin
/// no longer describes what the ladder would build.
pub(crate) fn current_ladder_digest(stems: &[&str]) -> Result<String, String> {
    let nodes = recipe_closure_full(stems)?;
    let mut rows: BTreeMap<String, String> = BTreeMap::new();
    for node in &nodes {
        rows.insert(
            format!("recipe {}", node.stem),
            node.recipe.to_json().to_canonical(),
        );
    }
    for input in classify_graph_inputs(&nodes)? {
        let pinned = match &input {
            SeedInput::LocalSource { path, .. } => format!("local {path}"),
            other => seed_digests::expected(other.key())?
                .unwrap_or("unpinned")
                .to_string(),
        };
        rows.insert(format!("seed {}", input.key()), pinned);
    }
    Ok(ladder_digest(
        rows.iter().map(|(k, v)| (k.as_str(), v.clone())),
    ))
}

/// The ABI token a pin builds the ladder under: never the compiled ABI, so
/// the root's paths are never a full climb's (`store::BUILDER_ABI`).
pub(crate) fn pin_abi(effective: &str) -> String {
    if effective.ends_with("-root") {
        effective.to_string()
    } else {
        format!("{effective}-root")
    }
}

/// Err unless the compiled pin still describes the ladder.
pub(crate) fn require_current() -> Result<&'static Root, String> {
    let root = root()?;
    let now = current_ladder_digest(CUT)?;
    if now != root.ladder {
        return Err(format!(
            "seed/bootstrap-root.txt was pinned from a different ladder (pinned {}, now {now}): \
             a rung below the gcc-14 cut, or a seed it stages, changed. Re-pin with \
             `td-recipe-eval bootstrap-root pin`, or build from stage0 with {FULL_ENV}=1.",
            root.ladder
        ));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder stems some recipe beyond the ladder's side builds from. By
    /// `ladder_side`'s definition these are always cut members; the test below
    /// holds CUT to exactly them, so the cut carries no unused export.
    fn used_cut() -> Result<Vec<String>, String> {
        let ladder = ladder()?;
        let mut used: BTreeSet<String> = BTreeSet::new();
        for (stem, recipe) in catalog::all() {
            if ladder_side(stem)? {
                continue;
            }
            for input in recipe
                .inputs
                .iter()
                .chain(recipe.native_inputs.iter())
                .chain(recipe.payload_inputs.iter())
                .flatten()
            {
                if ladder.contains(input) {
                    used.insert(input.clone());
                }
            }
        }
        Ok(used.into_iter().collect())
    }

    #[test]
    fn the_cut_is_exactly_what_post_cut_recipes_build_from() {
        let cut: Vec<String> = CUT.iter().map(|s| s.to_string()).collect();
        let mut sorted = cut.clone();
        sorted.sort();
        assert_eq!(cut, sorted, "keep CUT sorted");
        assert_eq!(used_cut().unwrap(), cut);
    }

    #[test]
    fn no_export_is_also_a_seed_table_key() {
        for stem in CUT {
            assert_eq!(seed_digests::expected(stem).unwrap(), None, "{stem}");
            assert!(
                crate::local_source_roster::expected(stem)
                    .unwrap()
                    .is_none(),
                "{stem}"
            );
        }
    }

    #[test]
    fn a_pin_never_records_the_compiled_abi_token() {
        assert_eq!(pin_abi("5"), "5-root");
        assert_eq!(pin_abi("5-root"), "5-root");
    }

    #[test]
    fn the_compiled_root_exports_the_cut_and_describes_the_current_ladder() {
        let root = root().unwrap();
        if !root.is_pinned() {
            return;
        }
        let exports: Vec<&str> = root.exports.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(exports, CUT);
        // The pin must move with the ladder: a rung or seed change that does
        // not re-pin reds here rather than at a build that cannot find its
        // pinned paths.
        require_current().unwrap();
    }

    #[test]
    fn a_ladder_target_is_never_cut() {
        if !root().unwrap().is_pinned() {
            return;
        }
        assert!(cut_for_with(&["gcc-14"], false).unwrap().is_none());
        assert!(cut_for_with(&["glibc-mesboot"], false).unwrap().is_none());
        assert!(cut_for_with(&["gcc-10-bridge-test"], false)
            .unwrap()
            .is_none());
        let cut = cut_for_with(&["rust-toolchain"], false).unwrap().unwrap();
        assert!(cut.contains("gcc-14"));
        assert!(cut_for_with(&["rust-toolchain"], true).unwrap().is_none());
    }
}
