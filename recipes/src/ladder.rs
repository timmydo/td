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

use crate::types::{Step, TextEdit};

/// The td-built bootstrap shell (catalog stem). `bash-mesboot` is bash 2.05b
/// built from source with no host tools (baked Makefiles + engine-native
/// patches + `oyacc`), so every rung declares it as a RecipeOutput edge.
pub const BASH: &str = "bash-mesboot";

/// The td-built tcc-era userland (catalog stems) EVERY rung declares as its
/// scripting toolset. Each is the `-mesboot0` provider recipe built from source
/// under tcc + mes libc — coreutils/sed/grep/gawk/diffutils as
/// `AuditedSeed`/`RecipeOutput` edges, never bare host names.
///
/// GNU findutils is deliberately absent as an evidenced DEAD axis for this
/// bootstrap toolset. A later source build may expose BusyBox `find`/`xargs`
/// through a ToolFarm only when it declares `busybox-x86-64`; the
/// `no_bootstrap_step_invokes_host_find_or_xargs` guard below enforces that
/// provenance instead of permitting an ambient PATH lookup.
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
/// engine.
pub fn link_bins(binutils_rung: &str) -> Step {
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

/// The exact line the bootable-kernel rung's busybox `/init` prints on ttyS0 once
/// the kernel has reached userspace, and that the host-side `qemu-boot` tool asserts
/// on. SINGLE SOURCE OF TRUTH shared by the `/init` script, both initramfs shape
/// checks, and the boot tool (`checks/qemu_boot.rs`, via `td_recipe::ladder`), so the
/// producer, the gated shape check, and the boot oracle can never silently desync.
pub const USERLAND_MARKER: &str = "TD-USERLAND-OK";

/// Shell (for `sh -c`) asserting that `initramfs` is a COMPLETE, well-formed newc cpio
/// carrying the bootable busybox userland. Shared by the `linux-x86-64` producer rung
/// and the `linux-x86-64-test` rung so the two checks cannot drift.
///
/// Uses `busybox cpio -t` for a REAL newc parse: it reads records to the closing
/// `TRAILER!!!` and exits non-zero on a truncated/corrupt stream (`cpio: short read`),
/// and its listing is exact MEMBER NAMES. Both matter — the previous payload greps
/// (`grep -a TRAILER` / `grep -a busybox`) are satisfied by strings EMBEDDED IN THE
/// BUSYBOX BINARY itself (it contains both "TRAILER!!!" and "busybox"), so an archive
/// truncated after the marker but before its real trailer passed every assertion.
/// Matching cpio member names instead defeats that, and the parse itself catches the
/// truncation. The `{marker}` payload grep is kept because it proves the /init script's
/// CONTENT (not just its name) is packed — cpio -t validates structure, not bytes.
///
/// `busybox` is the absolute path to the busybox multi-call binary; `grep`/`od`/`wc`
/// come from the mesboot0 userland, so callers keep `PATH = mesboot0_path()`.
pub fn initramfs_cpio_shape_check(initramfs: &str, busybox: &str) -> String {
    let marker = USERLAND_MARKER;
    format!(
        "sz=$(wc -c < '{initramfs}'); \
         [ \"$sz\" -ge 65536 ] || {{ echo \"initramfs.cpio: implausibly small ($sz bytes) — the static busybox alone is ~1 MiB\" >&2; exit 1; }}; \
         set -- $(od -An -tx1 -N 6 '{initramfs}'); \
         [ \"$1$2$3$4$5$6\" = 303730373031 ] || {{ echo 'initramfs.cpio: missing the newc cpio magic 070701' >&2; exit 1; }}; \
         list=$('{busybox}' cpio -t < '{initramfs}' 2>/dev/null) || {{ echo 'initramfs.cpio: busybox cpio -t could not parse the archive (truncated/corrupt newc stream — no valid TRAILER)' >&2; exit 1; }}; \
         for m in init bin/busybox bin/sh dev/console; do \
             printf '%s\\n' \"$list\" | grep -q -x -F \"$m\" || {{ echo \"initramfs.cpio: cpio member '$m' missing — the bootable userland is incomplete\" >&2; exit 1; }}; \
         done; \
         grep -q -a {marker} '{initramfs}' || {{ echo 'initramfs.cpio: /init marker not packed — the boot script the qemu tool asserts on is missing' >&2; exit 1; }}"
    )
}

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
/// {src} unless absolute). `sed -i` writes a temp file and renames, so it never
/// touches stdin or a non-syncable fd — the mes-libc bugs sed-mesboot0 patches
/// don't apply here.
pub fn sed_i(expr: &str, files: &[&str]) -> Step {
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

/// Make libtool assemble a static library (e.g. libstdc++.a) from its
/// convenience archives WITHOUT `find` (re #469, #477's retired-axis guard).
///
/// `ltmain.sh`'s `func_extract_archives` merges each per-language convenience
/// archive (libc++11convenience.a &c.) into the final `.a` by `cd`-ing into a
/// scratch dir, `ar x`-ing the members flat into it, then enumerating them with
/// `find $my_xdir -name \*.o -print`. The mesboot userland ships no `find`
/// (retired in #477), so that enumeration returns nothing, `ar rc` appends
/// nothing, and the archive silently ends up with only its directly-compiled
/// objects — a partial libstdc++.a missing std::string/std::vector/iostream.
/// GCC's own C++ generators (gensupport, genattrtab under GCC 14) then fail to
/// link against it.
///
/// `ar x` extracts object members flat, one level deep (libtool's own `ar t`
/// pass aborts on duplicate member names within an archive), so a *terminal*
/// glob over `$my_xdir` captures exactly what the recursive `find` would — and
/// unlike a non-terminal glob it expands correctly under bash-mesboot (bash
/// 2.05b on mes libc). `test -f` drops the no-match literal; `printf '%s\n'`
/// prints one path per line, exactly like `find … -print`.
///
/// We replace only the `find` COMMAND, leaving libtool's surrounding backticks
/// and its `| [sort |] $NL2SP` post-pipe intact: that command is byte-identical
/// across the two libtool versions td builds (GCC 4.9.4 pipes `find … | $NL2SP`;
/// GCC 14.3.0 pipes `find … | sort | $NL2SP` for a deterministic archive), so
/// one edit serves both and 14.3.0 keeps its sort. The `count: 1` fail-closes if
/// a future source bump drifts the line. This ELIMINATES the find need rather
/// than satisfying it with a host/find provider.
pub fn libtool_extract_without_find(ltmain: &str) -> Step {
    Step::substitute_text(
        ltmain,
        vec![TextEdit::new(
            "find $my_xdir -name \\*.$objext -print -o -name \\*.lo -print",
            "for f in $my_xdir/*.$objext $my_xdir/*.lo; do test -f \"$f\" && printf '%s\\n' \"$f\"; done",
            1,
        )],
    )
}

/// Make GCC 14's libstdc++ stamp rules independent of the absent `date` tool.
///
/// The stamp contents are never read; make uses only each file's existence and
/// mtime. A shell-builtin no-op plus redirection therefore preserves the rule's
/// semantics without adding another bootstrap-userland executable. Patch both
/// the ordinary convenience archive and the optional debug-tree stamp so every
/// C++-enabled GCC 14 rung has the same host-free source shape.
pub fn gcc14_libstdcxx_stamp_fixups() -> Step {
    Step::substitute_text(
        "{src}/libstdc++-v3/src/Makefile.in",
        vec![
            TextEdit::new(
                "\tdate > stamp-libstdc++convenience;",
                "\t: > stamp-libstdc++convenience;",
                1,
            ),
            TextEdit::new("\tdate > stamp-debug;", "\t: > stamp-debug;", 1),
        ],
    )
}

/// Select GCC's cp-based include-tree installer when bootstrap `tar` is absent.
///
/// Modern GCC configure otherwise chooses `install-headers-tar` for the native
/// i686/x86_64 hosts used by this ladder. The source ships an equivalent
/// `install-headers-cp` target, backed by the already-declared mesboot coreutils.
pub fn gcc_install_headers_without_tar() -> Step {
    Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "INSTALL_HEADERS_DIR = @build_install_headers_dir@",
            "INSTALL_HEADERS_DIR = install-headers-cp",
            1,
        )],
    )
}

/// The bash-mesboot `configure` fixups every modern GCC rung needs before its
/// `configure` runs (re #469). bash 2.05b (mes libc) cannot expand the
/// non-terminal `*/config-lang.in` globs configure uses to discover language
/// front-ends, and its automake dependency-style probe runs each depmode as
/// `env $depcmd` but the mesboot userland ships no `env` (so every depmode exits
/// 127 and the probe aborts with "no usable dependency style found"). `LANGS`
/// is the exact, sorted set of language fragments shipped by the selected GCC
/// tarball. Pre-expand both globs to that set (a working shell's expansion
/// verbatim) and rewrite the probe to the POSIX builtin `eval "$depcmd"`.
/// `--enable-languages` still selects only what each rung asks for. The edit
/// counts fail-closed if a future source bump drifts.
pub fn gcc_configure_fixups(langs: &[&str]) -> Vec<Step> {
    let top = langs
        .iter()
        .map(|l| format!("${{srcdir}}/gcc/{l}/config-lang.in"))
        .collect::<Vec<_>>()
        .join(" ");
    let gcc = langs
        .iter()
        .map(|l| format!("${{srcdir}}/{l}/config-lang.in"))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        Step::substitute_text(
            "{src}/configure",
            vec![TextEdit::new("${srcdir}/gcc/*/config-lang.in", &top, 2)],
        ),
        Step::substitute_text(
            "{src}/gcc/configure",
            vec![TextEdit::new("${srcdir}/*/config-lang.in", &gcc, 2)],
        ),
        Step::substitute_text(
            "{src}/gcc/configure",
            vec![TextEdit::new("env $depcmd", "eval \"$depcmd\"", 1)],
        ),
        Step::substitute_text(
            "{src}/libcpp/configure",
            vec![TextEdit::new("env $depcmd", "eval \"$depcmd\"", 1)],
        ),
    ]
}

/// GCC 14.3.0 ships twelve language fragments. Every GCC 14 rung uses the same
/// release tarball, so this wrapper keeps their call sites declarative while the
/// shared implementation also serves the GCC 10.5.0 bridge.
pub fn gcc14_configure_fixups() -> Vec<Step> {
    gcc_configure_fixups(&[
        "ada", "c", "cp", "d", "fortran", "go", "jit", "lto", "m2", "objc", "objcp", "rust",
    ])
}

/// Disable GCC's build-host signal-name self-test. The bootstrap libc's
/// `sys_siglist` is deliberately a stub, so executing this development-only
/// diagnostic crashes even when the compiler itself is sound. Installed
/// compiler behavior is covered by rung-specific checks and downstream builds.
pub fn gcc_disable_selftest() -> Step {
    Step::substitute_text(
        "{src}/gcc/Makefile.in",
        vec![TextEdit::new(
            "all.internal: start.encap rest.encap doc selftest",
            "all.internal: start.encap rest.encap doc",
            1,
        )],
    )
}

/// Make glibc 2.41's architecture selection and syscall generation work with
/// the mesboot shell/userland. Its configure asks bash-mesboot to expand the
/// non-terminal `sysdeps/*/preconfigure` glob; that shell leaves it literal, so
/// x86_64 never becomes x86_64/64 and the matching arch-syscall.h is omitted.
/// Pre-expand the exact sorted fragment set shipped by the pinned release.
///
/// make-syscalls.sh also uses GNU grep's newer `-o` option to enumerate the
/// byte offsets of `U` argument markers, while the declared grep-mesboot0 2.4
/// predates that option. The awk loop emits the identical zero-based `N:U`
/// records from the same colon-prefixed signature using the already-declared
/// gawk provider. Finally, elf/Makefile repeats the non-terminal
/// `build/*/stamp.os` glob while generating librtld.mk; GNU make's wildcard
/// function supplies the same existing-file set without relying on the shell.
pub fn glibc241_host_free_fixups() -> Vec<Step> {
    let preconfigure = [
        "aarch64",
        "alpha",
        "arc",
        "arm",
        "csky",
        "hppa",
        "i386",
        "loongarch",
        "m68k",
        "microblaze",
        "mips",
        "or1k",
        "powerpc",
        "riscv",
        "s390",
        "sh",
        "sparc",
        "x86_64",
    ]
    .iter()
    .map(|arch| format!("${{srcdir}}/sysdeps/{arch}/preconfigure"))
    .collect::<Vec<_>>()
    .join(" ");
    vec![
        Step::substitute_text(
            "{src}/configure",
            vec![TextEdit::new(
                "$srcdir/sysdeps/*/preconfigure",
                &preconfigure,
                1,
            )],
        ),
        Step::substitute_text(
            "{src}/sysdeps/unix/make-syscalls.sh",
            vec![TextEdit::new(
                "grep -ob U",
                r#"awk '{ for (i = 1; i <= length($0); ++i) if (substr($0, i, 1) == "U") print i - 1 ":U" }'"#,
                1,
            )],
        ),
        Step::substitute_text(
            "{src}/elf/Makefile",
            vec![TextEdit::new(
                "$(common-objpfx)*/stamp.os",
                "$(wildcard $(common-objpfx)*/stamp.os)",
                1,
            )],
        ),
    ]
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
    /// Run step), ToolFarm links, and the `to` side of the literal SubstituteText
    /// edits (the host-free `patch`/`sed` stand-in). Engine-native steps that
    /// carry only paths (Unpack/CopyTree/Symlink/PatchShebangs/…) cannot invoke a
    /// tool, so they contribute nothing. Shared by the catalog-walk guard and its
    /// coverage test so both exercise exactly the same extraction.
    ///
    /// Only a SubstituteText's `to` is a command surface: `from` is the text being
    /// REMOVED from a source file, so a `find`/`xargs` there is being deleted, not
    /// invoked (e.g. the gcc-mesboot ltmain.sh edit that replaces libtool's
    /// convenience-archive `find` with a bash-mesboot glob loop). Scanning `from`
    /// would misfire on exactly the patches that eliminate a host-tool call.
    fn command_texts(step: &Step) -> Vec<&str> {
        match step {
            Step::Run { argv, .. } => argv.iter().map(String::as_str).collect(),
            Step::WriteFile { content, .. } => vec![content.as_str()],
            Step::ToolFarm { links } => links
                .iter()
                .flat_map(|(a, b)| [a.as_str(), b.as_str()])
                .collect(),
            Step::SubstituteText { edits, .. } => edits.iter().map(|e| e.to.as_str()).collect(),
            _ => Vec::new(),
        }
    }

    /// Dead-axis lock: GNU findutils is absent from the tool tier after an
    /// exhaustive sweep found no rung invokes ambient `find`/`xargs` (not in any Run
    /// argv, WriteFile body, ToolFarm link, or SubstituteText edit — and neither
    /// is in the autoconf `configure`/`make` vocabulary these tarballs drive).
    /// This walks the WHOLE catalog and fails if any rung reintroduces a host
    /// `find`/`xargs` invocation, which would silently need the removed PATH node
    /// back. A rung may expose one only through a ToolFarm link to an explicitly
    /// declared td-built BusyBox input; the Rust source build needs those tools.
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
                        let declared_busybox_tool =
                            recipe.native_inputs.as_ref().is_some_and(|inputs| {
                                inputs.iter().any(|input| input == "busybox-x86-64")
                            }) && matches!(
                                step,
                                Step::ToolFarm { links }
                                    if links.iter().any(|(name, target)| {
                                        name == cmd
                                            && target
                                                == "{in:busybox-x86-64}/bin/busybox"
                                    })
                            );
                        assert!(
                            !invokes(text, cmd) || declared_busybox_tool,
                            "recipe `{stem}' invokes `{cmd}' in `{text}' — \
                             GNU findutils was retired from the tool tier; a rung \
                             must expose this command through a ToolFarm link to \
                             its declared td-built busybox-x86-64 input"
                        );
                    }
                }
            }
        }
    }

    /// Proof that `command_texts` — the extraction the guard above runs — covers
    /// the interpreted-text surfaces that are NOT a `Run` argv: a baked
    /// Makefile/kaem script (`WriteFile`, `exec: false`) and the `to` side of a
    /// literal patch/sed edit (`SubstituteText`). Without this, a `find`/`xargs`
    /// reintroduced in one of those would slip past the guard.
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

    /// A SubstituteText's `from` is REMOVED text, not a command: a patch that
    /// deletes a `find`/`xargs` call (like the real `libtool_extract_without_find`
    /// ltmain.sh glob-loop swap) must not be flagged as reintroducing the tool.
    /// The guard scans only `to`, so a `find` in `from` with a tool-free `to` is
    /// allowed. Exercised against the actual helper so the two cannot drift.
    #[test]
    fn guard_ignores_find_on_the_removed_from_side() {
        let removes_find = super::libtool_extract_without_find("{src}/ltmain.sh");
        // The helper's `from` names `find`; its `to` (the glob loop) does not.
        assert!(
            !command_texts(&removes_find)
                .iter()
                .any(|t| invokes(t, "find")),
            "a find on the removed `from' side must not be flagged as an invocation"
        );
    }
}
