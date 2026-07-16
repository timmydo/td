//! Shared step builders for the bootstrap-ladder rungs (#378 slices 2+3).
//!
//! Every rung recipe (recipes/src/recipes/{mes,tcc,…}.rs) composes its typed
//! `Step` list from these helpers. Conventions:
//! - `BASH` is the td-built bootstrap shell (`bash-mesboot`, bash 2.05b built
//!   entirely from source — no host tools). Every rung that needs a POSIX shell
//!   declares it as a typed RecipeOutput edge, never the leaked host bash.
//! - `MESBOOT0_TOOLS` are the td-built tcc-era userland (coreutils/sed/grep/
//!   gawk/diffutils `-mesboot0` providers) EVERY rung declares as lock inputs;
//!   `mesboot0_path()` / `mesboot0_inputs()` lay them onto a rung's PATH and
//!   input list ({tools} farm first, then the td shell, then the providers).
//! - Unpacking is ENGINE-NATIVE (`Step::Unpack` — td's own std-only
//!   tar/gzip/bzip2/xz readers), so no rung declares an unpacker package.

use crate::types::Step;

/// The td-built bootstrap shell (catalog stem). `bash-mesboot` is bash 2.05b
/// built from source with no host tools (baked Makefiles + engine-native
/// patches + `oyacc`), so every rung declares it as a RecipeOutput edge.
pub const BASH: &str = "bash-mesboot";

/// The td-built tcc-era userland (catalog stems) EVERY rung declares as its
/// scripting toolset. Each is the `-mesboot0` provider recipe built from source
/// under tcc + mes libc — coreutils/sed/grep/gawk/diffutils as
/// `AuditedSeed`/`RecipeOutput` edges, never bare host names.
///
/// `findutils` is deliberately absent as an evidenced DEAD axis: no rung
/// declares it, no ToolFarm links `find`/`xargs`, and no baked script, patch, or
/// Makefile fragment invokes either — they are not in the autoconf
/// `configure`/`make` vocabulary these tarballs drive, so the PATH node would be
/// pure phantom ingress. The `no_bootstrap_step_invokes_host_find_or_xargs`
/// guard below locks it out.
pub const MESBOOT0_TOOLS: &[&str] = &[
    "coreutils-mesboot0",
    "sed-mesboot0",
    "grep-mesboot0",
    "gawk-mesboot0",
    "diffutils-mesboot0",
];

/// The rung PATH template: the `{tools}` farm first, then the td shell, then the
/// td-built `MESBOOT0_TOOLS` packages. Every Run step that needs the scripting
/// userland uses this.
pub fn mesboot0_path() -> String {
    let mut p = String::from("{tools}");
    p.push_str(&format!(":{{in:{BASH}}}/bin"));
    for t in MESBOOT0_TOOLS {
        p.push_str(&format!(":{{in:{t}}}/bin"));
    }
    p
}

/// A rung's full lock-input list: the rung-specific `extras` FIRST, then the td
/// shell `BASH`, then the td-built `MESBOOT0_TOOLS` — in lockstep with the order
/// `mesboot0_path()` lays down, so a rung's inputs cannot drift out of step with
/// the PATH nodes and red only at execution deep in the chain. Pair with
/// `Recipe::inputs_owned`.
pub fn mesboot0_inputs(extras: &[&str]) -> Vec<String> {
    extras
        .iter()
        .copied()
        .chain(std::iter::once(BASH))
        .chain(MESBOOT0_TOOLS.iter().copied())
        .map(|s| s.to_string())
        .collect()
}

/// The tool-farm step that symlinks a prior binutils rung's whole `bin/` into
/// `{tools}` (as/ld/ar/ranlib/nm/strip/…) with the td-built `coreutils-mesboot0`
/// `ln`, on `mesboot0_path()`. The `glob:` argv element expands sorted in the
/// engine. (The `_mesboot0` suffix is now redundant; renaming consumers is a
/// deferred mechanical cleanup.)
pub fn link_bins_mesboot0(binutils_rung: &str) -> Step {
    Step::run(
        "{root}",
        &[
            "{in:coreutils-mesboot0}/bin/ln",
            "-sf",
            &format!("glob:{{in:{binutils_rung}}}/bin/*"),
            "{tools}",
        ],
    )
    .env("PATH", &mesboot0_path())
}

/// The declared shell (the sandbox has no /bin/sh): the td-built `bash-mesboot`
/// output, not a host bash.
pub const SH: &str = "{in:bash-mesboot}/bin/bash";

/// Unpack tarball input NAME into DEST (top-level dir stripped) with the
/// ENGINE's own readers — no unpacker packages in the sandbox.
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

/// `sed -i EXPR FILE…` via the td-built `sed-mesboot0` on `mesboot0_path()` (dir
/// {src} unless absolute). `sed -i`
/// writes a temp file and renames, so it never touches stdin or a non-syncable
/// fd — the mes-libc bugs sed-mesboot0 patches don't apply here. (The `_mesboot0`
/// suffix is now redundant; renaming consumers is a deferred mechanical cleanup.)
pub fn sed_i_mesboot0(expr: &str, files: &[&str]) -> Step {
    let mut argv: Vec<&str> = vec!["{in:sed-mesboot0}/bin/sed", "-i", expr];
    argv.extend_from_slice(files);
    Step::run("{src}", &argv).env("PATH", &mesboot0_path())
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

#[cfg(test)]
mod tests {
    use crate::catalog;
    use crate::types::Step;

    /// True if `cmd` appears in `s` as a whole command word. Splitting on every
    /// non-alphanumeric char means `/usr/bin/find`, `find`, and `find;` all
    /// surface the word `find`, while `findutils`, `found`, and `x86-64` do not.
    fn invokes(s: &str, cmd: &str) -> bool {
        s.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| t == cmd)
    }

    /// Every catalog-authored text of a step that becomes a command or an
    /// interpreted script/Makefile: Run argv, ANY WriteFile body (baked
    /// Makefiles/kaem scripts are written `exec: false` and then run over by a
    /// Run step), ToolFarm links, and the literal SubstituteText edits (the
    /// host-free `patch`/`sed` stand-in). Engine-native steps that carry only
    /// paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a tool, so
    /// they contribute nothing. Shared by the catalog-walk guard and its
    /// coverage test so both exercise exactly the same extraction.
    fn command_texts(step: &Step) -> Vec<&str> {
        match step {
            Step::Run { argv, .. } => argv.iter().map(String::as_str).collect(),
            Step::WriteFile { content, .. } => vec![content.as_str()],
            Step::ToolFarm { links } => links
                .iter()
                .flat_map(|(a, b)| [a.as_str(), b.as_str()])
                .collect(),
            Step::SubstituteText { edits, .. } => edits
                .iter()
                .flat_map(|e| [e.from.as_str(), e.to.as_str()])
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Dead-axis lock: `findutils` is absent from the tool tier after an
    /// exhaustive sweep found no rung invokes `find`/`xargs` (not in any Run
    /// argv, WriteFile body, ToolFarm link, or SubstituteText edit — and neither
    /// is in the autoconf `configure`/`make` vocabulary these tarballs drive).
    /// This walks the WHOLE catalog and fails if any rung reintroduces a host
    /// `find`/`xargs` invocation, which would silently need the removed PATH node
    /// back. A future rung that legitimately needs `find` must declare a td-built
    /// provider output (findutils/busybox), never the host tool, and update this
    /// guard deliberately.
    ///
    /// Coverage note: it scans every catalog-authored surface that becomes a
    /// command or an interpreted script/Makefile — Run argv, ANY WriteFile body
    /// (baked Makefiles/kaem scripts are written `exec: false` and then run over
    /// by a Run step), ToolFarm links, and the literal SubstituteText edits (the
    /// host-free `patch`/`sed` stand-in). Engine-native steps that carry only
    /// paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a tool.
    #[test]
    fn no_bootstrap_step_invokes_host_find_or_xargs() {
        for (stem, recipe) in catalog::all() {
            let Some(steps) = &recipe.steps else {
                continue;
            };
            for step in steps {
                for text in command_texts(step) {
                    for cmd in ["find", "xargs"] {
                        assert!(
                            !invokes(text, cmd),
                            "recipe `{stem}' invokes `{cmd}' in `{text}' — \
                             findutils was retired from the tool tier as a dead \
                             axis; a rung that needs it must declare a \
                             td-built provider output (findutils/busybox) and update \
                             this guard deliberately, never lean on a host tool on PATH"
                        );
                    }
                }
            }
        }
    }

    /// Proof that `command_texts` — the extraction the guard above runs — covers
    /// the interpreted-text surfaces that are NOT a `Run` argv: a baked
    /// Makefile/kaem script (`WriteFile`, `exec: false`) and a literal patch/sed
    /// edit (`SubstituteText`). Without this, a `find`/`xargs` reintroduced in one
    /// of those would slip past the guard.
    #[test]
    fn guard_scans_nonexec_writefile_and_substitutetext() {
        use crate::types::TextEdit;

        let baked_makefile = Step::WriteFile {
            path: "Makefile".into(),
            content: "clean:\n\tfind . -name '*.o' -delete\n".into(),
            exec: false,
        };
        let literal_edit = Step::SubstituteText {
            file: "configure".into(),
            edits: vec![TextEdit::new("rm -f x", "xargs rm -f", 1)],
        };
        for (step, cmd) in [(&baked_makefile, "find"), (&literal_edit, "xargs")] {
            assert!(
                command_texts(step).iter().any(|t| invokes(t, cmd)),
                "command_texts must scan this surface for `{cmd}'"
            );
        }
    }
}
