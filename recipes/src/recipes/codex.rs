use crate::types::{
    CargoGitPackage, CargoGitSource, CargoSourcePatch, CheckRunner, Recipe, RecipeCheck, TextEdit,
};

// Build the Codex CLI from the reviewed upstream release source with td's
// source-built Rust/C/C++ platform. The upstream workspace lock records its
// internal packages as 0.0.0 while the release manifests inherit 0.148.0;
// recipes/locks/codex/Cargo.lock is the mechanically normalized lock Cargo
// produces from those same pinned source bytes. Registry crates retain their
// upstream checksums, and every Git dependency is a separate fixed-output
// commit archive copied into Cargo's offline vendor tree without invoking Git.
// Exact source patches keep Reqwest's non-TLS defaults while selecting
// Rustls/native roots throughout, including Sentry's transport. They also
// replace protoc-bin-vendored with the declared source-built protoc executable.
// This removes the otherwise unified native-tls/OpenSSL graph without dropping
// HTTP/2, charset, cookie, system-proxy, crash context, or release-health
// features, and keeps target prebuilts out of the build graph.
pub fn recipe() -> Recipe {
    Recipe::rust("codex", "0.148.0")
        .source_input("codex-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
            "protobuf-x86-64",
        ])
        .cargo_subdir("codex-rs")
        .cargo_package("codex-cli")
        .cargo_lock("recipes/locks/codex/Cargo.lock")
        .replace_cargo_lock()
        .cargo_source_patches(vec![
            CargoSourcePatch::new(
                "Cargo.toml",
                vec![TextEdit::new(
                    r#"reqwest = { version = "0.12", features = ["cookies"] }"#,
                    r#"reqwest = { version = "0.12", default-features = false, features = [
    "charset",
    "cookies",
    "http2",
    "rustls-tls-native-roots",
    "system-proxy",
] }"#,
                    1,
                )],
            ),
            CargoSourcePatch::new(
                "feedback/Cargo.toml",
                vec![TextEdit::new(
                    r#"sentry = { version = "0.46" }"#,
                    r#"sentry = { version = "0.46", default-features = false, features = ["backtrace", "contexts", "debug-images", "panic", "release-health", "reqwest", "rustls"] }"#,
                    1,
                )],
            ),
            CargoSourcePatch::new(
                "code-mode-protocol/Cargo.toml",
                vec![TextEdit::new("protoc-bin-vendored = \"3.2.0\"\n", "", 1)],
            ),
            CargoSourcePatch::new(
                "code-mode-protocol/build.rs",
                vec![TextEdit::new(
                    "config.protoc_executable(protoc_bin_vendored::protoc_bin_path()?);",
                    "config.protoc_executable(std::env::var(\"PROTOC\")?);",
                    1,
                )],
            ),
        ])
        .cargo_git_sources(vec![
            CargoGitSource::new(
                "git+https://github.com/openai-oss-forks/crossterm?rev=45fecb9508105988f42fe6ff0441783ed3717f92#45fecb9508105988f42fe6ff0441783ed3717f92",
                "codex-cargo-crossterm-source",
                vec![CargoGitPackage::new("crossterm", "0.29.0", ".")],
            ),
            CargoGitSource::new(
                "git+https://github.com/helix-editor/nucleo.git?rev=4253de9faabb4e5c6d81d946a5e35a90f87347ee#4253de9faabb4e5c6d81d946a5e35a90f87347ee",
                "codex-cargo-nucleo-source",
                vec![
                    CargoGitPackage::new("nucleo", "0.5.0", "."),
                    CargoGitPackage::new("nucleo-matcher", "0.3.1", "matcher"),
                ],
            ),
            CargoGitSource::new(
                "git+https://github.com/dzbarsky/rules_rust?rev=b56cbaa8465e74127f1ea216f813cd377295ad81#b56cbaa8465e74127f1ea216f813cd377295ad81",
                "codex-cargo-rules-rust-source",
                vec![CargoGitPackage::new("runfiles", "0.1.0", "rust/runfiles")],
            ),
            CargoGitSource::new(
                "git+https://github.com/openai-oss-forks/tokio-tungstenite?rev=0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186#0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186",
                "codex-cargo-tokio-tungstenite-source",
                vec![CargoGitPackage::new("tokio-tungstenite", "0.28.0", ".")],
            ),
            CargoGitSource::new(
                "git+https://github.com/openai-oss-forks/tungstenite-rs?rev=4fffad30fe373adbdcffab9545e9e9bf4f2fc19f#4fffad30fe373adbdcffab9545e9e9bf4f2fc19f",
                "codex-cargo-tungstenite-source",
                vec![CargoGitPackage::new("tungstenite", "0.27.0", ".")],
            ),
        ])
        .bins(&["codex"])
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check codex: build the source-pinned CLI and validate its daily-driver and debug surfaces"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run codex 1
"#,
        )
        .with_runner(CheckRunner::Codex)])
}

#[cfg(test)]
mod tests {
    use super::recipe;

    #[test]
    fn workspace_and_all_foreign_source_kinds_are_explicit() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("codex-source"));
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("codex-rs"));
        assert_eq!(recipe.cargo_package.as_deref(), Some("codex-cli"));
        assert_eq!(
            recipe.bins.as_deref(),
            Some(["codex".to_string()].as_slice())
        );
        assert_eq!(recipe.replace_cargo_lock, Some(true));
        let sources = recipe.cargo_git_sources.as_deref().unwrap_or_default();
        assert_eq!(sources.len(), 5);
        assert_eq!(
            sources
                .iter()
                .flat_map(|source| source.packages.iter())
                .map(|package| package.name.as_str())
                .collect::<Vec<_>>(),
            [
                "crossterm",
                "nucleo",
                "nucleo-matcher",
                "runfiles",
                "tokio-tungstenite",
                "tungstenite",
            ]
        );
        for source in sources {
            assert!(source.source.contains("?rev="));
            assert_eq!(
                source.source.rsplit_once('#').map(|(_, rev)| rev.len()),
                Some(40)
            );
            assert!(recipe
                .inputs
                .as_deref()
                .unwrap_or_default()
                .contains(&source.input));
        }
    }

    #[test]
    fn native_build_tools_are_declared() {
        let recipe = recipe();
        let inputs = recipe.native_inputs.as_deref().unwrap_or_default();
        for required in [
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
            "protobuf-x86-64",
        ] {
            assert!(
                inputs.iter().any(|input| input == required),
                "missing {required}"
            );
        }
        assert!(!inputs.iter().any(|input| input.contains("openssl")));
        let patches = recipe.cargo_source_patches.as_deref().unwrap_or_default();
        assert_eq!(
            patches
                .iter()
                .map(|patch| patch.file.as_str())
                .collect::<Vec<_>>(),
            [
                "Cargo.toml",
                "feedback/Cargo.toml",
                "code-mode-protocol/Cargo.toml",
                "code-mode-protocol/build.rs",
            ]
        );
    }

    #[test]
    fn committed_lock_contains_no_prebuilt_protoc_package() {
        let lock = include_str!("../../locks/codex/Cargo.lock");
        assert!(!lock.contains("protoc-bin-vendored"));
        assert_eq!(lock.matches("[[package]]").count(), 1335);
        assert_eq!(lock.matches("source = \"registry+").count(), 1189);
    }
}
