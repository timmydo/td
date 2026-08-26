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
pub const ASSEMBLY_EXCEPTIONS: [(&str, &str); 7] = [
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
    ("sshd", "aws-lc-sys 0.41.0 x86_64 assembly"),
];

/// Recipes whose linked outputs include the Rust runtime boundary. The glibc
/// and libgcc boundaries apply to every output passed to the target splitter;
/// this roster adds Rust/LLVM and is pinned against both Cargo and direct-rustc
/// recipes by the catalog tests.
pub const RUST_PROFILED_RECIPES: [&str; 22] = [
    "codex",
    "fd",
    "ripgrep",
    "rust-toolchain",
    "sshd",
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
/// contain standard-library/compiler-runtime code, and sshd adds aws-lc.
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
                || (*source == "sshd" && recipe == "sshd")
        })
        .collect()
}

/// The ordinary profiler reader accepts at most 32 MiB of line program per
/// object. A named producer exception may retain a larger, structurally checked
/// line program as producer evidence while td-profiler deliberately reports
/// source-line attribution unavailable and keeps function symbols.
pub const DEFAULT_PROFILE_LINE_SECTION_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineAttributionException {
    pub runtime_relative_path: &'static str,
    pub max_line_section_bytes: u64,
    pub max_companion_bytes: u64,
    pub reason: &'static str,
}

pub const LINE_ATTRIBUTION_EXCEPTIONS: [(&str, LineAttributionException); 1] = [(
    "codex",
    LineAttributionException {
        runtime_relative_path: "bin/codex",
        max_line_section_bytes: 160 * 1024 * 1024,
        max_companion_bytes: 256 * 1024 * 1024,
        reason:
            "Codex 0.148.0's ThinLTO line program is beyond td-profiler's bounded per-object reader",
    },
)];

pub fn line_attribution_exception(recipe: &str) -> Option<LineAttributionException> {
    LINE_ATTRIBUTION_EXCEPTIONS
        .iter()
        .find_map(|(output, exception)| (*output == recipe).then_some(*exception))
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
                ("sshd", "aws-lc-sys 0.41.0 x86_64 assembly"),
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
        assert_eq!(output_assembly_exceptions("sshd").len(), 4);
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
    fn oversized_line_attribution_is_a_single_named_boundary() {
        assert_eq!(DEFAULT_PROFILE_LINE_SECTION_BYTES, 32 * 1024 * 1024);
        assert_eq!(LINE_ATTRIBUTION_EXCEPTIONS.len(), 1);
        let codex = line_attribution_exception("codex").unwrap();
        assert_eq!(codex.runtime_relative_path, "bin/codex");
        assert_eq!(codex.max_line_section_bytes, 160 * 1024 * 1024);
        assert_eq!(codex.max_companion_bytes, 256 * 1024 * 1024);
        assert_eq!(
            codex.reason,
            "Codex 0.148.0's ThinLTO line program is beyond td-profiler's bounded per-object reader"
        );
        assert_eq!(line_attribution_exception("td-profiler"), None);
    }
}
