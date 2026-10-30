//! Target-wide compiler policy for shipped, source-built user-mode artifacts.
//!
//! This belongs in the shared engine rather than in an individual recipe: both
//! `td-builder`'s Cargo runner and `td-recipe`'s direct-rustc recipes consume the
//! exact same spellings. Host control-plane builds do not use this module.

/// rustc arguments used by direct target recipes. `{root}` and `{src}` are
/// expanded by the typed recipe engine so the ephemeral build directory never
/// reaches DWARF and package source has the canonical `/td-build` root. The
/// linker is not policy here: recipes name td-link (`td-link/DESIGN.md`).
/// The splitter drops the line tables; emitting them keeps each linked
/// runtime, and so its build ID, independent of what companions retain.
pub const DIRECT_RUSTC_ARGS: &[&str] = &[
    "-Cforce-frame-pointers=yes",
    "-Cdebuginfo=line-tables-only",
    "-Cstrip=none",
    "-Clink-arg=-Wl,--build-id=sha1",
    "--remap-path-prefix={root}=/td-build-root",
    "--remap-path-prefix={src}=/td-build",
];

/// Debug sections removed from every target companion after its ordinary
/// symbols have been copied out of the runtime: every DWARF section, the
/// line program and its string tables included. A companion keeps `.symtab`,
/// `.strtab` and the build-ID note. Keep the producers and the recipe-side
/// consumer guard on this one roster; the pair validator refuses any DWARF
/// section it misses.
pub const PRUNED_DEBUG_SECTIONS: &[&str] = &[
    ".debug_info",
    ".debug_abbrev",
    ".debug_aranges",
    ".debug_types",
    ".debug_ranges",
    ".debug_rnglists",
    ".debug_frame",
    ".debug_loc",
    ".debug_loclists",
    ".debug_str_offsets",
    ".debug_addr",
    ".debug_macro",
    ".debug_macinfo",
    ".debug_pubnames",
    ".debug_pubtypes",
    ".debug_gnu_pubnames",
    ".debug_gnu_pubtypes",
    ".debug_names",
    ".debug_sup",
    ".debug_cu_index",
    ".debug_tu_index",
    ".gdb_index",
    ".debug_line",
    ".debug_line_str",
    ".debug_str",
];

/// Materialize the direct-rustc policy for an independently rooted build. The
/// ordinary recipe path uses `{root}` and `{src}`; the bounded reproducibility
/// oracle supplies two different roots which must canonicalize to these same
/// target-visible names.
pub fn direct_rustc_args(build_root: &str, source_root: &str) -> Vec<String> {
    DIRECT_RUSTC_ARGS
        .iter()
        .map(|arg| {
            arg.replace("{root}=", &format!("{build_root}="))
                .replace("{src}=", &format!("{source_root}="))
        })
        .collect()
}

/// Reviewed prebuilt-code and assembly classes without certified frame chains.
/// Source-built compiler-generated functions around them use the global policy;
/// samples entering one of these ranges are an
/// explicit coverage boundary rather than silently trusted unwinds.
pub const ASSEMBLY_EXCEPTIONS: &[(&str, &str)] = &[
    (
        "glibc-x86-64",
        "Debian prebuilt glibc; frame pointers and symbols not guaranteed",
    ),
    (
        "gcc-runtime",
        "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed",
    ),
    // Every Rust output links upstream's prebuilt std, whose frame pointers
    // are incomplete, and its compiler-runtime assembly.
    (
        "rust-toolchain",
        "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime",
    ),
    // The target-built td-net multicall, the fetch service's tier, links
    // rustls through `ring`, whose pregenerated x86_64 assembly is compiled
    // by its build script (APPLICATIONS.md §W.8). The terminal applications
    // it serves are std alone and carry none.
    ("td-net", "ring 0.17.14 x86_64 assembly"),
];

/// Recipes whose linked outputs include the Rust runtime boundary. The glibc
/// and libgcc boundaries apply to every output passed to the target splitter;
/// this roster adds Rust/LLVM and is pinned against both Cargo and direct-rustc
/// recipes by the catalog tests.
pub const RUST_PROFILED_RECIPES: &[&str] = &[
    "fd",
    "ripgrep",
    "td-agent",
    "td-audio",
    "td-authd",
    "td-boot",
    "td-btrfs",
    "td-busd",
    "td-compositor",
    "td-dua",
    "td-editor",
    "td-firstboot",
    "td-init",
    "td-install",
    "td-install-qemu-test",
    "td-jail",
    "td-kexec",
    "td-link",
    "td-link-runtime",
    "td-link-test",
    "td-login",
    "td-luks",
    "td-mail",
    "td-net",
    "td-netd",
    "td-news",
    "td-open",
    "td-pass",
    "td-photo",
    "td-photo-test",
    "td-portal",
    "td-profiler",
    "td-review",
    "td-sandbox",
    "td-seatd",
    "td-secret",
    "td-secret-vm-test",
    "td-setup",
    "td-sh",
    "td-software",
    "td-svc",
    "td-taskmgr",
    "td-term",
    "td-time",
    "td-txt",
    "td-update",
    "td-util",
    "td-vm-guest",
    "uutils",
];

/// Build-root outputs that link gcc-runtime's start files and libgcc in
/// place of GCC's.
pub const BUILD_ROOT_LINKED: &[&str] = &[
    "zlib-x86-64",
    "openssh-x86-64",
    "git-x86-64",
    "curl-x86-64",
    "td-link",
];

/// Conservative transitive assembly boundaries for a linked output. Every
/// split target may contain glibc startup code; one linked in the build root
/// or by td-link also contains gcc-runtime's libgcc,
/// and Rust outputs the standard library and compiler runtime.
pub fn output_assembly_exceptions(recipe: &str) -> Vec<(&'static str, &'static str)> {
    ASSEMBLY_EXCEPTIONS
        .iter()
        .copied()
        .filter(|(source, _)| {
            *source == "glibc-x86-64"
                // Build-root C and every Rust output, which td-link links,
                // take gcc-runtime's start files and libgcc, not GCC's.
                || (*source == "gcc-runtime"
                    && (BUILD_ROOT_LINKED.contains(&recipe)
                        || RUST_PROFILED_RECIPES.contains(&recipe)))
                || (*source == "rust-toolchain" && RUST_PROFILED_RECIPES.contains(&recipe))
                // A package's own crate assembly reaches that package alone.
                || (*source == recipe && recipe == "td-net")
        })
        .collect()
}

/// Hash-visible producer policy for every recipe which creates debug
/// companions. The control-plane builder has a stable ABI identity, so a
/// semantic splitter change must move this token to re-key realized outputs.
pub const DEBUG_COMPANION_POLICY: &str = "symbols-v2";

/// The shipped image's 128-MiB companion budget, measured over the whole
/// deployment rather than by any one producer.
pub const DEPLOYMENT_DEBUG_CEILING_BYTES: u64 = 134_217_728;

/// Compose the Cargo runner's target-only RUSTFLAGS. The linker is Cargo's
/// target linker, td-link, which Cargo also gives build scripts and proc
/// macros. The three varying source
/// roots are mapped separately so dependencies compiled from the vendor tree do
/// not retain a sandbox pathname that the package-source mapping cannot cover.
pub fn cargo_rustflags(build_root: &str, cargo_root: &str, vendor_root: &str) -> String {
    format!(
        "--remap-path-prefix={build_root}=/td-build \
         --remap-path-prefix={cargo_root}=/td-cargo \
         --remap-path-prefix={vendor_root}=/td-cargo/vendor \
         -Cforce-frame-pointers=yes -Cdebuginfo=line-tables-only -Cstrip=none \
         -Clink-arg=-Wl,--build-id=sha1"
    )
}

/// C/C++ flags for native objects compiled by Cargo build scripts. These
/// objects become part of the shipped Rust ELF and therefore must preserve the
/// same frame chain and deterministic source naming as Rust code.
pub fn cargo_cflags(build_root: &str, cargo_root: &str, vendor_root: &str) -> String {
    format!(
        "-O2 -g1 -fno-omit-frame-pointer \
         -ffile-prefix-map={build_root}=/td-build \
         -ffile-prefix-map={cargo_root}=/td-cargo \
         -ffile-prefix-map={vendor_root}=/td-cargo/vendor"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruned_debug_roster_is_unique_and_includes_line_data() {
        let mut sections = PRUNED_DEBUG_SECTIONS.to_vec();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(sections.len(), PRUNED_DEBUG_SECTIONS.len());
        for pruned in [
            ".debug_info",
            ".debug_line",
            ".debug_line_str",
            ".debug_str",
        ] {
            assert!(PRUNED_DEBUG_SECTIONS.contains(&pruned), "{pruned}");
        }
        assert!(PRUNED_DEBUG_SECTIONS
            .iter()
            .all(|section| section.starts_with(".debug_") || *section == ".gdb_index"));
        assert_eq!(DEBUG_COMPANION_POLICY, "symbols-v2");
    }

    #[test]
    fn target_flags_pin_frames_lines_identity_and_all_varying_roots() {
        let rust = cargo_rustflags("/tmp/build", "/tmp/cargo", "/tmp/vendor");
        for required in [
            "-Cforce-frame-pointers=yes",
            "-Cdebuginfo=line-tables-only",
            "-Cstrip=none",
            "-Clink-arg=-Wl,--build-id=sha1",
            "--remap-path-prefix=/tmp/build=/td-build",
            "--remap-path-prefix=/tmp/cargo=/td-cargo",
            "--remap-path-prefix=/tmp/vendor=/td-cargo/vendor",
        ] {
            assert!(
                rust.contains(required),
                "missing target Rust policy: {required}"
            );
        }
        // td-link is the linker, named outside the policy; no lld opt-out.
        assert!(!rust.contains("linker"), "{rust}");
        assert!(!DIRECT_RUSTC_ARGS.iter().any(|arg| arg.contains("linker")));
        let c = cargo_cflags("/tmp/build", "/tmp/cargo", "/tmp/vendor");
        for required in [
            "-fno-omit-frame-pointer",
            "-g1",
            "-ffile-prefix-map=/tmp/build=/td-build",
            "-ffile-prefix-map=/tmp/cargo=/td-cargo",
            "-ffile-prefix-map=/tmp/vendor=/td-cargo/vendor",
        ] {
            assert!(c.contains(required), "missing target C policy: {required}");
        }
        assert_eq!(
            direct_rustc_args("/scratch/one", "/source/one"),
            [
                "-Cforce-frame-pointers=yes",
                "-Cdebuginfo=line-tables-only",
                "-Cstrip=none",
                "-Clink-arg=-Wl,--build-id=sha1",
                "--remap-path-prefix=/scratch/one=/td-build-root",
                "--remap-path-prefix=/source/one=/td-build",
            ]
        );
    }

    #[test]
    fn assembly_coverage_boundaries_are_an_explicit_roster() {
        assert_eq!(
            ASSEMBLY_EXCEPTIONS,
            [
                ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime"
                ),
                ("td-net", "ring 0.17.14 x86_64 assembly"),
            ]
        );
        // The toolchain is the boundary's source, not one of its outputs.
        assert_eq!(
            output_assembly_exceptions("rust-toolchain"),
            vec![(
                "glibc-x86-64",
                "Debian prebuilt glibc; frame pointers and symbols not guaranteed"
            )]
        );
        assert_eq!(DEPLOYMENT_DEBUG_CEILING_BYTES, 134_217_728);
        assert_eq!(
            output_assembly_exceptions("td-boot"),
            vec![
                ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime"
                ),
            ]
        );
        assert_eq!(
            output_assembly_exceptions("td-net"),
            vec![
                ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime"
                ),
                ("td-net", "ring 0.17.14 x86_64 assembly"),
            ]
        );
        // The terminal applications are std alone: only the boundaries every
        // Rust output has, and no entry of their own.
        for application in ["td-mail", "td-news"] {
            assert_eq!(
                output_assembly_exceptions(application),
                vec![
                    ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                    ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                    (
                        "rust-toolchain",
                        "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime"
                    ),
                ],
                "{application}"
            );
        }
        assert!(!output_assembly_exceptions("ripgrep")
            .iter()
            .any(|(source, _)| *source == "td-net"));
        assert_eq!(
            output_assembly_exceptions("glibc-x86-64"),
            vec![(
                "glibc-x86-64",
                "Debian prebuilt glibc; frame pointers and symbols not guaranteed"
            ),]
        );
        // td-link builds in the root and is a Rust output.
        assert_eq!(
            output_assembly_exceptions("td-link"),
            vec![
                ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.99.1 prebuilt standard library and compiler-runtime"
                ),
            ]
        );
        for stem in BUILD_ROOT_LINKED
            .iter()
            .filter(|stem| !RUST_PROFILED_RECIPES.contains(stem))
        {
            assert_eq!(
                output_assembly_exceptions(stem),
                vec![
                    ("glibc-x86-64", "Debian prebuilt glibc; frame pointers and symbols not guaranteed"),
                    ("gcc-runtime", "Debian prebuilt libgcc and startup objects; frame pointers and symbols not guaranteed"),
                ],
                "{stem}"
            );
        }
    }
}
