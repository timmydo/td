//! Target-wide compiler policy for shipped, source-built user-mode artifacts.
//!
//! This belongs in the shared engine rather than in an individual recipe: both
//! `td-builder`'s Cargo runner and `td-recipe`'s direct-rustc recipes consume the
//! exact same spellings. Host control-plane builds do not use this module.

/// rustc arguments used by direct target recipes. `{root}` and `{src}` are
/// expanded by the typed recipe engine so the ephemeral build directory never
/// reaches DWARF and package source has the canonical `/td-build` root.
pub const DIRECT_RUSTC_ARGS: [&str; 7] = [
    "-Cforce-frame-pointers=yes",
    "-Cdebuginfo=line-tables-only",
    "-Cstrip=none",
    "-Clink-arg=-Wl,--build-id=sha1",
    // Upstream's gnu target links through rust-lld by default; recipes keep
    // the declared GNU linker until the rust-lld link driver replaces it.
    "-Clinker-features=-lld",
    "--remap-path-prefix={root}=/td-build-root",
    "--remap-path-prefix={src}=/td-build",
];

/// Debug sections removed from every target companion after its line program
/// and ordinary symbols have been copied out of the runtime. Keep the producer
/// and recipe-side consumer guard on this one roster.
pub const ALWAYS_PRUNED_DEBUG_SECTIONS: &[&str] = &[
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
];

/// Materialize the direct-rustc policy for an independently rooted build. The
/// ordinary recipe path uses `{root}` and `{src}`; the bounded reproducibility
/// oracle supplies two different roots which must canonicalize to these same
/// target-visible names.
pub fn direct_rustc_args(build_root: &str, source_root: &str) -> [String; 7] {
    [
        DIRECT_RUSTC_ARGS[0].into(),
        DIRECT_RUSTC_ARGS[1].into(),
        DIRECT_RUSTC_ARGS[2].into(),
        DIRECT_RUSTC_ARGS[3].into(),
        DIRECT_RUSTC_ARGS[4].into(),
        format!("--remap-path-prefix={build_root}=/td-build-root"),
        format!("--remap-path-prefix={source_root}=/td-build"),
    ]
}

/// Reviewed source classes whose hand-written assembly is not certified to
/// preserve an x86-64 frame chain. Compiler-generated functions around them
/// still use the global policy; samples entering one of these ranges are an
/// explicit coverage boundary rather than silently trusted unwinds.
pub const ASSEMBLY_EXCEPTIONS: &[(&str, &str)] = &[
    ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
    (
        "gcc-x86-64-stage1",
        "upstream stage1 GCC libgcc x86_64 assembly",
    ),
    (
        "gcc-x86-64-native",
        "upstream native GCC libgcc x86_64 assembly",
    ),
    ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
    // libgcc built in the Debian build root.
    ("gcc-runtime", "upstream GCC libgcc x86_64 assembly"),
    // Every Rust output links upstream's prebuilt std, whose frame pointers
    // are incomplete, and its compiler-runtime assembly.
    (
        "rust-toolchain",
        "upstream Rust 1.96.0 prebuilt standard library and compiler-runtime",
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
    "td-cc",
    "td-compositor",
    "td-dua",
    "td-editor",
    "td-firstboot",
    "td-init",
    "td-install",
    "td-install-qemu-test",
    "td-jail",
    "td-kexec",
    "td-login",
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
    "td-seatd",
    "td-secret",
    "td-secret-vm-test",
    "td-setup",
    "td-sh",
    "td-svc",
    "td-taskmgr",
    "td-term",
    "td-txt",
    "td-update",
    "td-util",
    "td-vm-guest",
    "uutils",
];

/// Build-root outputs that link gcc-runtime's start files and libgcc in
/// place of GCC's.
pub const BUILD_ROOT_LINKED: &[&str] = &["gcc-runtime", "zlib-x86-64", "cryptsetup-x86-64"];

/// Conservative transitive assembly boundaries for a linked output. Every
/// split target may contain glibc startup code and libgcc; Rust outputs also
/// contain standard-library/compiler-runtime code.
pub fn output_assembly_exceptions(recipe: &str) -> Vec<(&'static str, &'static str)> {
    ASSEMBLY_EXCEPTIONS
        .iter()
        .copied()
        .filter(|(source, _)| {
            *source == "glibc-x86-64"
                || (*source == "gcc-x86-64-stage1" && recipe == "glibc-x86-64")
                || (*source == "gcc-x86-64-native"
                    && matches!(recipe, "binutils-x86-64-self" | "gcc-x86-64-self"))
                || (*source == "gcc-x86-64-self"
                    && !matches!(recipe, "glibc-x86-64" | "binutils-x86-64-self")
                    && !BUILD_ROOT_LINKED.contains(&recipe))
                || (*source == "gcc-runtime" && BUILD_ROOT_LINKED.contains(&recipe))
                || (*source == "rust-toolchain" && RUST_PROFILED_RECIPES.contains(&recipe))
                // A package's own crate assembly reaches that package alone.
                || (*source == recipe && recipe == "td-net")
        })
        .collect()
}

/// The ordinary profiler reader accepts at most 32 MiB of line program per
/// object. A named producer exception may retain a larger, structurally checked
/// line program as producer evidence while td-profiler deliberately reports
/// source-line attribution unavailable and keeps function symbols.
pub const DEFAULT_PROFILE_LINE_SECTION_BYTES: u64 = 32 * 1024 * 1024;

/// Hash-visible producer policy for every recipe which creates debug
/// companions. The control-plane builder has a stable ABI identity, so a
/// semantic splitter change must move this token to re-key realized outputs.
pub const DEBUG_COMPANION_POLICY: &str = "line-tables-v2";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineAttributionException {
    pub runtime_relative_path: &'static str,
    pub max_line_section_bytes: u64,
    pub max_companion_bytes: u64,
    pub require_complete_line_strings: bool,
    pub reason: &'static str,
}

/// Empty since td ships the upstream Rust toolchain, which has no companions;
/// a producer whose line program outgrows the reader is named here.
pub const LINE_ATTRIBUTION_EXCEPTIONS: &[(&str, LineAttributionException)] = &[];

pub fn line_attribution_exception(recipe: &str) -> Option<LineAttributionException> {
    LINE_ATTRIBUTION_EXCEPTIONS
        .iter()
        .find_map(|(output, exception)| (*output == recipe).then_some(*exception))
}

/// Complete derivation-hashed splitter policy for one recipe. The version
/// moves for global transform changes; every exception field is included so a
/// local admission change cannot reuse an output produced under older bounds.
pub fn debug_companion_policy(recipe: &str) -> String {
    match line_attribution_exception(recipe) {
        Some(exception) => format!(
            "{DEBUG_COMPANION_POLICY};runtime={};line={};companion={};complete-line-strings={};reason={}",
            exception.runtime_relative_path,
            exception.max_line_section_bytes,
            exception.max_companion_bytes,
            exception.require_complete_line_strings,
            exception.reason,
        ),
        None => DEBUG_COMPANION_POLICY.to_string(),
    }
}

/// The shipped image's one-GiB companion budget, measured over the whole
/// deployment rather than by any one producer.
pub const DEPLOYMENT_DEBUG_CEILING_BYTES: u64 = 1_073_741_824;

/// Compose the Cargo runner's target-only RUSTFLAGS. The three varying source
/// roots are mapped separately so dependencies compiled from the vendor tree do
/// not retain a sandbox pathname that the package-source mapping cannot cover.
pub fn cargo_rustflags(
    build_root: &str,
    cargo_root: &str,
    vendor_root: &str,
    linker: &str,
) -> String {
    format!(
        "--remap-path-prefix={build_root}=/td-build \
         --remap-path-prefix={cargo_root}=/td-cargo \
         --remap-path-prefix={vendor_root}=/td-cargo/vendor \
         -Cforce-frame-pointers=yes -Cdebuginfo=line-tables-only -Cstrip=none \
         -Clink-arg=-Wl,--build-id=sha1 -Clinker-features=-lld -Clinker={linker}"
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
    fn always_pruned_debug_roster_is_unique_and_excludes_line_data() {
        let mut sections = ALWAYS_PRUNED_DEBUG_SECTIONS.to_vec();
        sections.sort_unstable();
        sections.dedup();
        assert_eq!(sections.len(), ALWAYS_PRUNED_DEBUG_SECTIONS.len());
        assert!(ALWAYS_PRUNED_DEBUG_SECTIONS.contains(&".debug_info"));
        for not_always_pruned in [".debug_line", ".debug_line_str", ".debug_str"] {
            assert!(!ALWAYS_PRUNED_DEBUG_SECTIONS.contains(&not_always_pruned));
        }
    }

    #[test]
    fn target_flags_pin_frames_lines_identity_and_all_varying_roots() {
        let rust = cargo_rustflags("/tmp/build", "/tmp/cargo", "/tmp/vendor", "/td/gcc");
        for required in [
            "-Cforce-frame-pointers=yes",
            "-Cdebuginfo=line-tables-only",
            "-Cstrip=none",
            "-Clink-arg=-Wl,--build-id=sha1",
            "-Clinker-features=-lld",
            "--remap-path-prefix=/tmp/build=/td-build",
            "--remap-path-prefix=/tmp/cargo=/td-cargo",
            "--remap-path-prefix=/tmp/vendor=/td-cargo/vendor",
        ] {
            assert!(
                rust.contains(required),
                "missing target Rust policy: {required}"
            );
        }
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
                "-Clinker-features=-lld",
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
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                (
                    "gcc-x86-64-stage1",
                    "upstream stage1 GCC libgcc x86_64 assembly"
                ),
                (
                    "gcc-x86-64-native",
                    "upstream native GCC libgcc x86_64 assembly"
                ),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                ("gcc-runtime", "upstream GCC libgcc x86_64 assembly"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.96.0 prebuilt standard library and compiler-runtime"
                ),
                ("td-net", "ring 0.17.14 x86_64 assembly"),
            ]
        );
        // The toolchain is the boundary's source, not one of its outputs.
        assert_eq!(
            output_assembly_exceptions("rust-toolchain"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
            ]
        );
        assert_eq!(DEPLOYMENT_DEBUG_CEILING_BYTES, 1_073_741_824);
        assert_eq!(
            output_assembly_exceptions("td-boot"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.96.0 prebuilt standard library and compiler-runtime"
                ),
            ]
        );
        assert_eq!(
            output_assembly_exceptions("td-net"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                (
                    "rust-toolchain",
                    "upstream Rust 1.96.0 prebuilt standard library and compiler-runtime"
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
                    ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                    ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                    (
                        "rust-toolchain",
                        "upstream Rust 1.96.0 prebuilt standard library and compiler-runtime"
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
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                (
                    "gcc-x86-64-stage1",
                    "upstream stage1 GCC libgcc x86_64 assembly"
                ),
            ]
        );
        assert_eq!(
            output_assembly_exceptions("binutils-x86-64-self"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                (
                    "gcc-x86-64-native",
                    "upstream native GCC libgcc x86_64 assembly"
                ),
            ]
        );
        assert_eq!(
            output_assembly_exceptions("gcc-x86-64-self"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                (
                    "gcc-x86-64-native",
                    "upstream native GCC libgcc x86_64 assembly"
                ),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
            ]
        );
        for stem in BUILD_ROOT_LINKED {
            assert_eq!(
                output_assembly_exceptions(stem),
                vec![
                    ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                    ("gcc-runtime", "upstream GCC libgcc x86_64 assembly"),
                ],
                "{stem}"
            );
        }
    }

    #[test]
    fn oversized_line_attribution_boundaries_are_an_explicit_roster() {
        assert_eq!(DEFAULT_PROFILE_LINE_SECTION_BYTES, 32 * 1024 * 1024);
        assert_eq!(DEBUG_COMPANION_POLICY, "line-tables-v2");
        assert!(LINE_ATTRIBUTION_EXCEPTIONS.is_empty());
        assert_eq!(line_attribution_exception("rust-toolchain"), None);
        assert_eq!(line_attribution_exception("td-profiler"), None);
        assert_eq!(debug_companion_policy("td-profiler"), "line-tables-v2");
        let mut recipes = std::collections::BTreeSet::new();
        for (recipe, exception) in LINE_ATTRIBUTION_EXCEPTIONS {
            assert!(recipes.insert(recipe), "duplicate exception for {recipe}");
            assert!(DEFAULT_PROFILE_LINE_SECTION_BYTES < exception.max_line_section_bytes);
            assert!(exception.max_line_section_bytes <= exception.max_companion_bytes);
            let policy = debug_companion_policy(recipe);
            assert!(policy.contains(exception.runtime_relative_path));
            assert!(policy.contains(&exception.max_line_section_bytes.to_string()));
            assert!(policy.contains(&exception.max_companion_bytes.to_string()));
            assert!(policy.contains(exception.reason));
        }
    }
}
