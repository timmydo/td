use crate::shared::{BUILD_ROOT_GLIBC, BUILD_ROOT_PATH as PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// rust-toolchain-test: the shape of the shipped upstream Rust toolchain and
// of the Rust userland it builds, graded in the Debian build root.
//
// The toolchain: every executable the rust-dist transform relinks names td
// glibc's loader, its lib/ links reach td glibc, gcc-runtime's libgcc_s and
// td zlib, gcc-ld holds only ld.lld (rust-lld, which reports LLD), no debug
// companions or /gnu/store bytes ride along, and upstream's licence notices
// do. Then it works: rustc links a program through td-link whose .comment
// names rust-lld, Cargo builds a crate offline, and Clippy passes a clean
// crate, denies `unwrap()` under `--deny clippy::unwrap_used` and passes
// the repaired one (recipes/src/probes/rust_clippy.rs).
//
// The userland: ripgrep, fd and uutils' multicall each name td glibc's
// loader, carry a debug companion with the runtime's Build ID, a runtime
// with no .symtab and a companion with .symtab and no DWARF, the
// assembly-exception marker naming glibc, gcc-runtime and the prebuilt std,
// and no /gnu/store bytes. The rust-toolchain check's `td shell` proof
// runs them; this recipe holds their ELF.
//
// Every step runs the root's bash with PATH the root's /usr/bin alone, and
// readelf and grep are the root's: no subject is on PATH, and each is
// reached by its store path. rustc and Cargo are the subject, so every
// rustc compile and the Cargo leg run with the toolchain's own bin as
// PATH, which is where Cargo finds cargo-clippy, and nothing of the
// root's (no `cc`) to fall back on.

/// The shipped executables the transform relinks onto td glibc's loader.
const RELINKED: &[&str] = &[
    "bin/rustc",
    "bin/rustdoc",
    "bin/cargo",
    "bin/cargo-clippy",
    "bin/clippy-driver",
    "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld",
];

/// The Rust userland and the binary of each the proofs run.
const PRODUCTS: &[(&str, &str)] = &[("ripgrep", "rg"), ("fd", "fd"), ("uutils", "coreutils")];

/// Compiles a lint-free crate and one using `unwrap()` under
/// `--deny clippy::unwrap_used`, then the repaired one.
const CLIPPY_PROBE: &str = include_str!("../probes/rust_clippy.rs");

/// The prelude every script starts with: strict bash, the root's readelf,
/// and td glibc's loader. The root has a loader, libc and `cc` of its own,
/// so a smoke program that runs proves nothing about its link: `td_linked`
/// requires td glibc's loader, which td-link writes and the root's gcc
/// does not.
const PRELUDE: &str = "set -euo pipefail\n\
fail() { echo \"$*\" >&2; exit 1; }\n\
readelf=/usr/bin/readelf\n\
interp='@GLIBC@/lib/ld-linux-x86-64.so.2'\n\
td_linked() { \"$readelf\" -l \"$1\" | grep -F \"[Requesting program interpreter: $interp]\" >/dev/null || fail \"$1 is not on td glibc's loader: td-link did not link it\"; }\n";

const TOOLCHAIN_SHAPE: &str = r#"test ! -e /gnu/store || fail '/gnu/store is reachable'
cd '{in:rust-toolchain}'
for b in @RELINKED@; do
  test -x "$b" || fail "$b is missing from rust-toolchain"
  "$readelf" -l "$b" | grep -F "[Requesting program interpreter: $interp]" >/dev/null || fail "$b is not on td glibc's loader"
done
for pair in 'libc.so.6:@GLIBC@/lib/libc.so.6' 'libgcc_s.so.1:{in:gcc-runtime}/lib/libgcc_s.so.1' 'libz.so.1:{in:zlib-x86-64}/lib/libz.so.1'; do
  test "$(readlink "lib/${pair%%:*}")" = "${pair#*:}" || fail "lib/${pair%%:*} is not td's"
done
gccld=lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld
test "$(readlink "$gccld/ld.lld")" = ../rust-lld || fail 'gcc-ld/ld.lld is not rust-lld'
test "$(ls "$gccld")" = ld.lld || fail 'gcc-ld holds more than ld.lld'
"$gccld/ld.lld" --version | grep -F LLD >/dev/null || fail 'gcc-ld/ld.lld does not report LLD'
test ! -e lib/debug || fail 'the upstream toolchain carries debug companions'
test -f share/doc/rust/COPYRIGHT.html || fail 'the upstream licence notices are missing'
rc=0; grep -R -a -F -l /gnu/store bin lib >/dev/null || rc=$?
[ "$rc" = 1 ] || fail 'rust-toolchain embeds /gnu/store bytes'
"#;

const RUSTC_SMOKE: &str = r#"cd '{root}/rustc-smoke'
PATH='{in:rust-toolchain}/bin' '{in:rust-toolchain}/bin/rustc' --edition=2021 main.rs -C linker='{in:td-link}/bin/td-link' -o smoke
td_linked smoke
test "$(./smoke)" = 42 || fail 'the rustc smoke program did not print 42'
c=$("$readelf" -p .comment smoke) || fail 'readelf -p .comment failed on the rustc smoke program'
case $c in *'rustc version'*) ;; *) fail 'the link has no rustc comment';; esac
case $c in *'Linker: LLD'*) ;; *) fail 'the td-link link did not use rust-lld';; esac
"#;

const CARGO_SMOKE: &str = r#"PATH='{in:rust-toolchain}/bin' RUSTC='{in:rust-toolchain}/bin/rustc' \
  CARGO_HOME='{root}/cargo-home' HOME='{root}/cargo-home' \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER='{in:td-link}/bin/td-link' \
  '{in:rust-toolchain}/bin/cargo' build --offline \
  --manifest-path '{root}/cargo-smoke/Cargo.toml' --target-dir '{root}/cargo-target'
td_linked '{root}/cargo-target/debug/td-cargo-smoke'
test "$('{root}/cargo-target/debug/td-cargo-smoke')" = 43 || fail 'the Cargo smoke program did not print 43'
"#;

const CLIPPY: &str = r#"PATH='{in:rust-toolchain}/bin' '{in:rust-toolchain}/bin/rustc' --edition=2021 '{root}/clippy-probe.rs' -C linker='{in:td-link}/bin/td-link' -o '{root}/clippy-probe-bin'
td_linked '{root}/clippy-probe-bin'
cd '{root}/clippy-probe'
out=$(PATH='{in:rust-toolchain}/bin' '{root}/clippy-probe-bin' '{in:rust-toolchain}/bin/cargo' '{in:rust-toolchain}/bin/rustc' '{in:td-link}/bin/td-link') || fail "the Clippy probe failed: $out"
case $out in *CLIPPY-PROBE-OK*) ;; *) fail "the Clippy probe did not finish: $out";; esac
"#;

const PRODUCT: &str = r#"package='{in:@PKG@}'
binary="$package/bin/@BIN@"
debug="$package/lib/debug/bin/@BIN@.debug"
test -x "$binary" || fail '@PKG@: bin/@BIN@ is missing'
"$readelf" -l "$binary" | grep -F "[Requesting program interpreter: $interp]" >/dev/null || fail "@PKG@ is not on td glibc's loader"
test -f "$debug" || fail '@PKG@: the debug companion is missing'
id=$("$readelf" -n "$binary" | grep 'Build ID:') || fail '@PKG@ has no Build ID'
debug_id=$("$readelf" -n "$debug" | grep 'Build ID:') || fail "@PKG@'s companion has no Build ID"
test "$id" = "$debug_id" || fail "@PKG@'s companion has another Build ID"
sections=$("$readelf" -S -W "$binary") || fail 'readelf -S failed on @PKG@'
rc=0; printf '%s\n' "$sections" | grep -F .symtab >/dev/null || rc=$?
[ "$rc" = 1 ] || fail '@PKG@ ships a .symtab'
sections=$("$readelf" -S -W "$debug") || fail "readelf -S failed on @PKG@'s companion"
printf '%s\n' "$sections" | grep -E '\] \.symtab +SYMTAB ' >/dev/null || fail "@PKG@'s companion has no .symtab"
rc=0; printf '%s\n' "$sections" | grep -E '\] \.(z?debug_[a-z_]*|gdb_index) +PROGBITS ' >/dev/null || rc=$?
[ "$rc" = 1 ] || fail "@PKG@'s companion carries DWARF"
marker="$package/lib/debug/.td-assembly-exception"
for line in exception.0.source=glibc-x86-64 exception.1.source=gcc-runtime exception.2.source=rust-toolchain; do
  grep -F -x "$line" "$marker" >/dev/null || fail "@PKG@'s assembly-exception marker lacks $line"
done
rc=0; grep -a -F /gnu/store "$binary" >/dev/null || rc=$?
[ "$rc" = 1 ] || fail '@PKG@ embeds /gnu/store bytes'
"#;

/// One step: the root's bash running `body` after the prelude.
fn bash(body: &str) -> Step {
    let script = format!("{PRELUDE}{body}")
        .replace("@GLIBC@", BUILD_ROOT_GLIBC)
        .replace("@RELINKED@", &RELINKED.join(" "));
    Step::run("{root}", &[SH, "-c", &script]).env("PATH", PATH)
}

pub fn recipe() -> Recipe {
    let mut steps = Vec::new();
    for path in [
        "{out}",
        "{root}/rustc-smoke",
        "{root}/cargo-smoke/src",
        "{root}/cargo-home",
        "{root}/clippy-probe",
    ] {
        steps.push(Step::MkDir { path: path.into() });
    }
    for (path, content) in [
        (
            "{root}/rustc-smoke/main.rs",
            "fn main() { println!(\"42\"); }\n",
        ),
        (
            "{root}/cargo-smoke/Cargo.toml",
            "[package]\nname = \"td-cargo-smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
        ),
        (
            "{root}/cargo-smoke/src/main.rs",
            "fn main() { println!(\"43\"); }\n",
        ),
        ("{root}/clippy-probe.rs", CLIPPY_PROBE),
    ] {
        steps.push(Step::WriteFile {
            path: path.into(),
            content: content.into(),
            exec: false,
        });
    }
    for body in [TOOLCHAIN_SHAPE, RUSTC_SMOKE, CARGO_SMOKE, CLIPPY] {
        steps.push(bash(body));
    }
    for (package, binary) in PRODUCTS {
        steps.push(bash(
            &PRODUCT.replace("@PKG@", package).replace("@BIN@", binary),
        ));
    }
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: upstream Rust 1.99.1 runs on td glibc, gcc-runtime and zlib, links through td-link and rust-lld, builds with Cargo offline and lints with Clippy; ripgrep, fd and uutils name td glibc's loader and carry Build-ID-matched debug companions\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("rust-toolchain-test", "1.0")
        .build_root()
        .native_inputs(INPUTS)
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check rust-toolchain-test: in the Debian build root, upstream Rust 1.99.1's relinked executables, lib links and gcc-ld are td's, rustc links through td-link and rust-lld, Cargo builds offline, Clippy denies unwrap(), and ripgrep, fd and uutils carry td glibc's loader and Build-ID-matched debug companions"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run rust-toolchain-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

/// The toolchain, its linker and runtime libraries, and the userland it
/// builds.
const INPUTS: &[&str] = &[
    "rust-toolchain",
    "td-link",
    "glibc-x86-64",
    "gcc-runtime",
    "zlib-x86-64",
    "ripgrep",
    "fd",
    "uutils",
];

#[cfg(test)]
mod tests {
    use crate::types::Step;

    /// The toolchain and the userland are graded only by the root's bash
    /// and tools: the runtime libraries are inputs for their paths alone.
    #[test]
    fn the_toolchain_is_graded_only_by_the_roots_bash_and_tools() {
        crate::shared::assert_build_root_test(
            &super::recipe(),
            super::INPUTS,
            &["rust-toolchain", "td-link", "ripgrep", "fd", "uutils"],
        );
    }

    /// Every relinked executable and every product binary is checked, and
    /// no template placeholder survives into a script.
    #[test]
    fn every_relinked_executable_and_product_is_checked() {
        let recipe = super::recipe();
        let scripts: Vec<String> = recipe
            .steps
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|step| match step {
                Step::Run { argv, .. } => argv.get(2).cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(scripts.len(), 4 + super::PRODUCTS.len());
        for script in &scripts {
            assert!(!script.contains('@'), "{script}");
            assert!(script.starts_with("set -euo pipefail\n"), "{script}");
        }
        let shape = scripts.first().unwrap();
        for binary in super::RELINKED {
            assert!(shape.contains(binary), "{binary}");
        }
        for (package, binary) in super::PRODUCTS {
            assert!(
                scripts
                    .iter()
                    .any(|s| s.contains(&format!("{{in:{package}}}"))
                        && s.contains(&format!("/bin/{binary}\""))),
                "{package}"
            );
        }
    }
}
