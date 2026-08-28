//! Target-wide compiler policy for shipped, source-built user-mode artifacts.
//!
//! This belongs in the shared engine rather than in an individual recipe: both
//! `td-builder`'s Cargo runner and `td-recipe`'s direct-rustc recipes consume the
//! exact same spellings. Host control-plane builds do not use this module.

/// rustc arguments used by direct target recipes. `{root}` and `{src}` are
/// expanded by the typed recipe engine so the ephemeral build directory never
/// reaches DWARF and package source has the canonical `/td-build` root.
pub const DIRECT_RUSTC_ARGS: [&str; 6] = [
    "-Cforce-frame-pointers=yes",
    "-Cdebuginfo=line-tables-only",
    "-Cstrip=none",
    "-Clink-arg=-Wl,--build-id=sha1",
    "--remap-path-prefix={root}=/td-build-root",
    "--remap-path-prefix={src}=/td-build",
];

/// Debug sections removed from every target companion after its line program
/// and ordinary symbols have been copied out of the runtime. Keep the producer
/// and recipe-side consumer guard on this one roster.
pub const ALWAYS_PRUNED_DEBUG_SECTIONS: [&str; 22] = [
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
pub fn direct_rustc_args(build_root: &str, source_root: &str) -> [String; 6] {
    [
        DIRECT_RUSTC_ARGS[0].into(),
        DIRECT_RUSTC_ARGS[1].into(),
        DIRECT_RUSTC_ARGS[2].into(),
        DIRECT_RUSTC_ARGS[3].into(),
        format!("--remap-path-prefix={build_root}=/td-build-root"),
        format!("--remap-path-prefix={source_root}=/td-build"),
    ]
}

/// Reviewed source classes whose hand-written assembly is not certified to
/// preserve an x86-64 frame chain. Compiler-generated functions around them
/// still use the global policy; samples entering one of these ranges are an
/// explicit coverage boundary rather than silently trusted unwinds.
pub const ASSEMBLY_EXCEPTIONS: [(&str, &str); 6] = [
    (
        "codex",
        "aws-lc-sys 0.39.0, ring 0.17.14, and zstd-sys 2.0.16+zstd.1.5.7 x86_64 assembly",
    ),
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
    (
        "rust-toolchain",
        "upstream LLVM and Rust compiler-runtime assembly",
    ),
];

/// Recipes whose linked outputs include the Rust runtime boundary. The glibc
/// and libgcc boundaries apply to every output passed to the target splitter;
/// this roster adds Rust/LLVM and is pinned against both Cargo and direct-rustc
/// recipes by the catalog tests.
pub const RUST_PROFILED_RECIPES: [&str; 21] = [
    "codex",
    "fd",
    "ripgrep",
    "rust-toolchain",
    "td-boot",
    "td-busd",
    "td-compositor",
    "td-firstboot",
    "td-init",
    "td-install",
    "td-jail",
    "td-kexec",
    "td-login",
    "td-netd",
    "td-profiler",
    "td-seatd",
    "td-sh",
    "td-svc",
    "td-txt",
    "td-util",
    "uutils",
];

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
                    && !matches!(recipe, "glibc-x86-64" | "binutils-x86-64-self"))
                || (*source == "rust-toolchain" && RUST_PROFILED_RECIPES.contains(&recipe))
                || (*source == "codex" && recipe == "codex")
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

pub const LINE_ATTRIBUTION_EXCEPTIONS: [(&str, LineAttributionException); 2] = [
    (
        "codex",
        LineAttributionException {
            runtime_relative_path: "bin/codex",
            max_line_section_bytes: 160 * 1024 * 1024,
            max_companion_bytes: 256 * 1024 * 1024,
            require_complete_line_strings: false,
            reason:
                "Codex 0.148.0's ThinLTO line program is beyond td-profiler's bounded per-object reader",
        },
    ),
    (
        "rust-toolchain",
        LineAttributionException {
            runtime_relative_path: "lib/librustc_driver-277b25caa34f5853.so",
            max_line_section_bytes: 128 * 1024 * 1024,
            max_companion_bytes: 192 * 1024 * 1024,
            require_complete_line_strings: true,
            reason:
                "Rust 1.96.0 librustc_driver's line program is beyond td-profiler's bounded per-object reader",
        },
    ),
];

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

/// Maximum debug-companion bytes admitted by the source-built Rust toolchain
/// recipe. The toolchain recipe owns its measurement; keeping the reviewed
/// literal here prevents measuring code from moving its own limit. Four GiB
/// leaves headroom for line tables from the source-built rustc/LLVM closure
/// without permitting unbounded full DWARF.
pub const TOOLCHAIN_DEBUG_CEILING_BYTES: u64 = 4_294_967_296;

/// The shipped image excludes the compiler toolchain, so its companions have a
/// separate one-GiB budget. This is deliberately not coupled to the much larger
/// LLVM/rustc ceiling: growth in one scope must not silently relax the other.
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
         -Clink-arg=-Wl,--build-id=sha1 -Clinker={linker}"
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
                (
                    "codex",
                    "aws-lc-sys 0.39.0, ring 0.17.14, and zstd-sys 2.0.16+zstd.1.5.7 x86_64 assembly",
                ),
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
                (
                    "rust-toolchain",
                    "upstream LLVM and Rust compiler-runtime assembly"
                ),
            ]
        );
        assert_eq!(TOOLCHAIN_DEBUG_CEILING_BYTES, 4_294_967_296);
        assert_eq!(DEPLOYMENT_DEBUG_CEILING_BYTES, 1_073_741_824);
        assert_eq!(
            output_assembly_exceptions("td-boot"),
            vec![
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                (
                    "rust-toolchain",
                    "upstream LLVM and Rust compiler-runtime assembly"
                ),
            ]
        );
        assert_eq!(
            output_assembly_exceptions("codex"),
            vec![
                (
                    "codex",
                    "aws-lc-sys 0.39.0, ring 0.17.14, and zstd-sys 2.0.16+zstd.1.5.7 x86_64 assembly",
                ),
                ("glibc-x86-64", "upstream glibc sysdeps/x86_64 assembly"),
                ("gcc-x86-64-self", "upstream GCC libgcc x86_64 assembly"),
                (
                    "rust-toolchain",
                    "upstream LLVM and Rust compiler-runtime assembly"
                ),
            ]
        );
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
    }

    #[test]
    fn oversized_line_attribution_boundaries_are_exactly_named() {
        assert_eq!(DEFAULT_PROFILE_LINE_SECTION_BYTES, 32 * 1024 * 1024);
        assert_eq!(DEBUG_COMPANION_POLICY, "line-tables-v2");
        assert_eq!(LINE_ATTRIBUTION_EXCEPTIONS.len(), 2);
        let codex = line_attribution_exception("codex").unwrap();
        assert_eq!(codex.runtime_relative_path, "bin/codex");
        assert_eq!(codex.max_line_section_bytes, 160 * 1024 * 1024);
        assert_eq!(codex.max_companion_bytes, 256 * 1024 * 1024);
        assert!(!codex.require_complete_line_strings);
        assert_eq!(
            codex.reason,
            "Codex 0.148.0's ThinLTO line program is beyond td-profiler's bounded per-object reader"
        );
        let rust = line_attribution_exception("rust-toolchain").unwrap();
        assert_eq!(
            rust.runtime_relative_path,
            "lib/librustc_driver-277b25caa34f5853.so"
        );
        assert_eq!(rust.max_line_section_bytes, 128 * 1024 * 1024);
        assert_eq!(rust.max_companion_bytes, 192 * 1024 * 1024);
        assert!(rust.require_complete_line_strings);
        assert_eq!(
            rust.reason,
            "Rust 1.96.0 librustc_driver's line program is beyond td-profiler's bounded per-object reader"
        );
        assert_eq!(line_attribution_exception("td-profiler"), None);
        assert_eq!(
            debug_companion_policy("td-profiler"),
            "line-tables-v2"
        );
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
