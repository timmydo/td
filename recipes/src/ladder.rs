//! Shared step builders for the bootstrap-ladder rungs (#378 slices 2+3).
//!
//! Every rung recipe (recipes/src/recipes/{mes,tcc,…}.rs) composes its typed
//! `Step` list from these helpers. Conventions:
//! - `BASH` is the td-built bootstrap shell (`bash-mesboot`, bash 2.05b built
//!   entirely from source — no host tools). Every rung that needs a POSIX shell
//!   declares it as a typed RecipeOutput edge, never the leaked host bash (#469).
//! - `BASE_TOOLS` are the remaining guix-seed host tools EVERY rung declares as
//!   lock inputs (replacing the deleted ladder's `ls /gnu/store/*pkg*`
//!   scavenging and the curated-PATH farm): coreutils, text tools.
//! - `base_path()` is the PATH template for Run steps: the rung's own {tools}
//!   farm first (prior-rung compilers/tools), then the td shell, then the base
//!   tool packages.
//! - Unpacking is ENGINE-NATIVE (`Step::Unpack` — td's own std-only
//!   tar/gzip/bzip2/xz readers), so no rung declares an unpacker package.

use crate::types::Step;

/// The td-built bootstrap shell (catalog stem). `bash-mesboot` is bash 2.05b
/// built from source with no host tools (baked Makefiles + engine-native
/// patches + `oyacc`), so every rung declares it as a RecipeOutput edge — the
/// first host tool retired from `BASE_TOOLS` in the #469 cutover.
pub const BASH: &str = "bash-mesboot";

/// The host-tool packages every rung declares (lock input names). Shrinks as
/// the cutover (re #469) replaces each host edge with a recipe output or an
/// engine-native step; the shell already left for `BASH` (bash-mesboot) and
/// tar/gzip/bzip2/xz already left with `Step::Unpack`.
pub const BASE_TOOLS: &[&str] = &[
    "coreutils",
    "sed",
    "grep",
    "gawk",
    "findutils",
    "diffutils",
];

/// The rung PATH template: the {tools} farm first, then the td shell, then the
/// base packages.
pub fn base_path() -> String {
    let mut p = String::from("{tools}");
    p.push_str(&format!(":{{in:{BASH}}}/bin"));
    for t in BASE_TOOLS {
        p.push_str(&format!(":{{in:{t}}}/bin"));
    }
    p
}

/// A rung's full lock-input list: the rung-specific `extras` FIRST, then the
/// td shell `BASH`, then the shared `BASE_TOOLS` — in lockstep with the order
/// `base_path()` lays down (`{tools}` then `{in:BASH}/bin` then the base
/// packages). Keeping the base set here (not re-typed per recipe) closes the
/// drift hazard: a rung's inputs must stay in lockstep with the constant
/// `base_path()` templates `{in:X}/bin` for, or the missing tool reds only at
/// execution, deep in the chain. Pair with `Recipe::inputs_owned`.
pub fn base_inputs(extras: &[&str]) -> Vec<String> {
    extras
        .iter()
        .copied()
        .chain(std::iter::once(BASH))
        .chain(BASE_TOOLS.iter().copied())
        .map(|s| s.to_string())
        .collect()
}

/// The tool-farm step that symlinks a prior binutils rung's whole `bin/` into
/// `{tools}` (as/ld/ar/ranlib/nm/strip/…) with the declared coreutils `ln`, on
/// `base_path()`. The `glob:` argv element expands sorted in the engine.
pub fn link_bins(binutils_rung: &str) -> Step {
    Step::run(
        "{root}",
        &[
            "{in:coreutils}/bin/ln",
            "-sf",
            &format!("glob:{{in:{binutils_rung}}}/bin/*"),
            "{tools}",
        ],
    )
    .env("PATH", &base_path())
}

/// The declared shell (the sandbox has no /bin/sh): the td-built `bash-mesboot`
/// output, not a host bash.
pub const SH: &str = "{in:bash-mesboot}/bin/bash";

/// Unpack tarball input NAME into DEST (top-level dir stripped) with the
/// ENGINE's own readers — no unpacker packages in the sandbox (re #469).
pub fn unpack_into(input: &str, dest: &str) -> Vec<Step> {
    vec![Step::Unpack {
        input: format!("{{in:{input}}}"),
        dest: dest.into(),
        keep_top: false,
    }]
}

/// Unpack tarball input NAME into DEST with the top-level dir KEPT (the gcc
/// prereqs land as gmp-X.Y.Z/ subdirs that then get version-free symlinks).
pub fn unpack_keep_top(input: &str, dest: &str) -> Vec<Step> {
    vec![Step::Unpack {
        input: format!("{{in:{input}}}"),
        dest: dest.into(),
        keep_top: true,
    }]
}

/// Apply a patch input with the td-built patch rung: `patch --force -p1 -i X`
/// in {src}, env-cleared (exactly the ladder's `env -i patch …`).
pub fn apply_patch(patch_rung: &str, patch_input: &str) -> Step {
    Step::run(
        "{src}",
        &[
            &format!("{{in:{patch_rung}}}/bin/patch"),
            "--force",
            "-p1",
            "-i",
            &format!("{{in:{patch_input}}}"),
        ],
    )
}

/// `sed -i EXPR FILE…` via the declared sed (dir {src} unless absolute).
pub fn sed_i(expr: &str, files: &[&str]) -> Step {
    let mut argv: Vec<&str> = vec!["{in:sed}/bin/sed", "-i", expr];
    argv.extend_from_slice(files);
    Step::run("{src}", &argv).env("PATH", &base_path())
}

/// Relocate every staged glibc GNU ld script under `lib/*.so` to bare member
/// names by stripping the configured store prefix. Real ELF shared objects are
/// left untouched.
pub fn relocate_ld_scripts(stage: &str, store_prefix: &str) -> Step {
    Step::RelocateLdScripts {
        dir: format!("{stage}/lib"),
        prefix: store_prefix.into(),
    }
}
