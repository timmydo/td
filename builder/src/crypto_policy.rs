//! Named dependency admission for the mail cryptography boundary.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

pub(crate) const DEPENDENCIES: [&str; 3] = [
    "aws-lc-rs = { version = \"=1.18.1\", default-features = false, features = [\"alloc\", \"non-fips\"] }",
    "rustls = { version = \"=0.23.45\", default-features = false, features = [\"std\", \"tls12\", \"aws_lc_rs\"] }",
    "webpki-roots = \"=1.0.8\"",
];

pub(crate) fn admitted(name: &str) -> bool {
    matches!(name, "td-crypto" | "td-mta")
}

pub(crate) fn manifest_pin(name: &str, text: &str) -> Result<(), String> {
    let expected = match name {
        "td-crypto" => "7ca2d70176ddb80083ff07de51465e8194fd01e4e4d435201444f11ed997c308",
        "td-mta" => "11804cce27bb14cb91d835e4f23bd46528b248e4ade98655f1ffacb63cb1553a",
        _ => {
            return Err(format!(
                "{name} has no external crypto dependency admission"
            ))
        }
    };
    check_pin(name, "Cargo.toml", text, expected)
}

pub(crate) fn lock_pin(name: &str, text: &str) -> Result<(), String> {
    let expected = match name {
        "td-crypto" => "499bfd9b6780ca6cc7df5c928a16d7397c43b61bbde1d164d532c494c530514b",
        "td-mta" => "832f4b9d3354d46a16c61806290b1e3799d72f91790a33daaec61b37695ce63a",
        _ => {
            return Err(format!(
                "{name} has no external crypto dependency admission"
            ))
        }
    };
    check_pin(name, "Cargo.lock", text, expected)
}

fn check_pin(name: &str, file: &str, text: &str, expected: &str) -> Result<(), String> {
    let actual = crate::sha256::hex_digest(text.as_bytes());
    if actual != expected {
        return Err(format!(
            "{name}/{file} differs from the reviewed crypto admission pin; review the manifest, lock and feature closure before updating this policy"
        ));
    }
    Ok(())
}

/// Cargo can inject environment controls and replace a native `links` build
/// script without changing its package graph. Ancestor configs must carry the
/// same reviewed pin; a private CARGO_HOME alone does not hide parent configs.
pub(crate) fn cargo_config(root: &Path) -> Result<(), String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("crypto repo root: {e}"))?;
    for directory in root.ancestors() {
        for name in ["config", "config.toml"] {
            let path = directory.join(".cargo").join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) if directory == root && name == "config.toml" => {}
                Ok(_) if name == "config.toml" => check_cargo_config(&path)?,
                Ok(_) => {
                    return Err(format!(
                        "crypto build refuses additional Cargo config: {}",
                        path.display()
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(format!(
                        "inspect crypto Cargo config {}: {e}",
                        path.display()
                    ))
                }
            }
        }
    }
    // An ancestor must not substitute for the checkout's own runner config.
    check_cargo_config(&root.join(".cargo/config.toml"))
}

fn check_cargo_config(path: &Path) -> Result<(), String> {
    const MAX_CONFIG_BYTES: u64 = 4096;
    let metadata = std::fs::metadata(path)
        .map_err(|e| format!("read crypto Cargo config {}: {e}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "crypto Cargo config must be a regular file of at most {MAX_CONFIG_BYTES} bytes: {}",
            path.display()
        ));
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_CONFIG_BYTES + 1).read_to_string(&mut text))
        .map_err(|e| format!("read crypto Cargo config {}: {e}", path.display()))?;
    check_pin(
        "repository",
        ".cargo/config.toml",
        &text,
        "6328f2aeb929deef4faff8e6108c0b6b07eac36ae98dbde30a5f37ac3b6ed225",
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn no_build_script(root: &Path, name: &str) -> Result<(), String> {
    match std::fs::symlink_metadata(root.join(name).join("build.rs")) {
        Ok(_) => Err(format!("{name} has no admitted build.rs")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect {name} build.rs: {e}")),
    }
}

// Cargo metadata can include inactive optional edges. Cargo tree's selected
// normal/build graph is checked separately from the complete locked sources.
const ACTIVE: &str = "aws-lc-rs v1.18.1|alloc,aws-lc-sys,non-fips,prebuilt-nasm
aws-lc-sys v0.45.0|prebuilt-nasm
cc v1.5.1|parallel
cmake v0.1.58|
dunce v1.0.5|
find-msvc-tools v0.1.14|
fs_extra v1.3.0|
jobserver v0.1.35|
libc v0.2.189|default,std
once_cell v1.21.4|alloc,race,std
pkg-config v0.3.34|
rustls v0.23.45|aws-lc-rs,aws_lc_rs,std,tls12
rustls-pki-types v1.15.1|alloc,default,std
rustls-webpki v0.103.15|alloc,aws-lc-rs,std
shlex v2.0.1|default,std
subtle v2.6.1|
untrusted v0.9.0|
webpki-roots v1.0.8|
zeroize v1.9.0|alloc,default
";

pub(crate) fn active_graph(root: &Path, name: &str, output: &str) -> Result<(), String> {
    if !admitted(name) {
        return Err(format!(
            "{name} has no external crypto dependency admission"
        ));
    }
    let mut expected: BTreeSet<String> = ACTIVE.lines().map(str::to_owned).collect();
    let mut local = vec!["td-crypto"];
    if name == "td-mta" {
        local.push("td-mta");
    }
    for package in local {
        let path = root
            .join(package)
            .canonicalize()
            .map_err(|e| format!("resolve {package} source: {e}"))?;
        let path = path.to_str().ok_or("crypto source path is not UTF-8")?;
        expected.insert(format!("{package} v0.1.0 ({path})|"));
    }
    let actual: BTreeSet<String> = output.lines().map(str::to_owned).collect();
    if actual != expected {
        let unexpected: Vec<_> = actual.difference(&expected).collect();
        let missing: Vec<_> = expected.difference(&actual).collect();
        return Err(format!(
            "{name} active crypto graph differs: unexpected {unexpected:?}; missing {missing:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf()
    }

    #[test]
    fn cargo_config_and_automatic_build_scripts_are_closed_inputs() {
        let root = root();
        let Ok(config) = std::fs::read_to_string(root.join(".cargo/config.toml")) else {
            return;
        };
        let base = std::env::temp_dir().join(format!("td-crypto-config-{}", std::process::id()));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        std::fs::create_dir(&base).unwrap();
        let _cleanup = Cleanup(base.clone());
        let base = base.canonicalize().unwrap();
        let checkout = base.join("checkout");
        std::fs::create_dir_all(checkout.join(".cargo")).unwrap();
        let path = checkout.join(".cargo/config.toml");
        std::fs::write(&path, &config).unwrap();
        assert!(cargo_config(&checkout).is_ok());
        for extra in [
            "[env]\nAWS_LC_SYS_NO_ASM = \"1\"\n",
            "[env]\nAWS_LC_SYS_USE_SYSTEM_x86_64_unknown_linux_gnu = \"1\"\n",
            "[env]\nCC = { value = \"/unapproved/cc\", force = true }\n",
            "[target.x86_64-unknown-linux-gnu.aws_lc_0_45_0]\nrustc-link-lib = [\"crypto\"]\n",
        ] {
            std::fs::write(&path, format!("{config}\n{extra}")).unwrap();
            assert!(cargo_config(&checkout).is_err());
        }
        std::fs::write(&path, &config).unwrap();
        for directory in [&checkout, &base] {
            std::fs::create_dir_all(directory.join(".cargo")).unwrap();
            let extra = directory.join(".cargo/config");
            std::fs::write(&extra, "[env]\nAWS_LC_SYS_NO_ASM = \"1\"\n").unwrap();
            assert!(cargo_config(&checkout).is_err());
            std::fs::remove_file(extra).unwrap();
        }
        let parent = base.join(".cargo/config.toml");
        std::fs::write(&parent, "[env]\nAWS_LC_SYS_NO_ASM = \"1\"\n").unwrap();
        assert!(cargo_config(&checkout).is_err());
        std::fs::remove_file(&parent).unwrap();
        assert!(cargo_config(&checkout).is_ok());

        let nested = checkout.join(".claude/worktrees/nested");
        std::fs::create_dir_all(nested.join(".cargo")).unwrap();
        let nested_config = nested.join(".cargo/config.toml");
        std::fs::write(&nested_config, &config).unwrap();
        assert!(cargo_config(&nested).is_ok(), "identical parent config");
        std::fs::write(&parent, &config).unwrap();
        assert!(
            cargo_config(&nested).is_ok(),
            "multiple identical ancestors"
        );
        for ancestor in [&parent, &path, &nested_config] {
            std::fs::write(ancestor, format!("{config}\n[env]\nCC = \"unapproved\"\n")).unwrap();
            let error = cargo_config(&nested).unwrap_err();
            assert!(error.contains("reviewed crypto admission pin"), "{error}");
            assert!(error.contains(&ancestor.display().to_string()), "{error}");
            std::fs::remove_file(ancestor).unwrap();
            std::fs::create_dir(ancestor).unwrap();
            assert!(
                cargo_config(&nested).is_err(),
                "unreadable config directory"
            );
            std::fs::remove_dir(ancestor).unwrap();
            std::os::unix::fs::symlink("absent", ancestor).unwrap();
            assert!(cargo_config(&nested).is_err(), "dangling config symlink");
            std::fs::remove_file(ancestor).unwrap();
            std::os::unix::fs::symlink("/dev/zero", ancestor).unwrap();
            let error = cargo_config(&nested).unwrap_err();
            assert!(error.contains("must be a regular file"), "{error}");
            std::fs::remove_file(ancestor).unwrap();
            std::fs::write(ancestor, vec![b'x'; 4097]).unwrap();
            let error = cargo_config(&nested).unwrap_err();
            assert!(error.contains("at most 4096 bytes"), "{error}");
            std::fs::write(ancestor, &config).unwrap();
        }
        for directory in [&base, &checkout, &nested] {
            let legacy = directory.join(".cargo/config");
            std::fs::write(&legacy, &config).unwrap();
            let error = cargo_config(&nested).unwrap_err();
            assert!(error.contains("refuses additional Cargo config"), "{error}");
            assert!(error.contains(&legacy.display().to_string()), "{error}");
            std::fs::remove_file(legacy).unwrap();
        }
        std::fs::remove_file(&nested_config).unwrap();
        let error = cargo_config(&nested).unwrap_err();
        assert!(error.contains("read crypto Cargo config"), "{error}");
        assert!(
            error.contains(&nested_config.display().to_string()),
            "{error}"
        );
        std::fs::write(&nested_config, &config).unwrap();
        assert!(cargo_config(&nested).is_ok());
        for name in ["td-crypto", "td-mta"] {
            std::fs::create_dir(checkout.join(name)).unwrap();
            assert!(no_build_script(&checkout, name).is_ok());
            let script = checkout.join(name).join("build.rs");
            std::fs::write(&script, "fn main() {}\n").unwrap();
            assert!(no_build_script(&checkout, name).is_err());
            std::fs::remove_file(&script).unwrap();
            std::os::unix::fs::symlink("absent", &script).unwrap();
            assert!(no_build_script(&checkout, name).is_err());
        }
    }

    #[test]
    fn exact_admission_rejects_manifest_and_lock_mutations() {
        let root = root();
        if !root.join("td-crypto/Cargo.toml").exists() {
            eprintln!("SKIP: builder-only source tree has no crypto admission inputs");
            return;
        }
        for name in ["td-crypto", "td-mta"] {
            let manifest = std::fs::read_to_string(root.join(name).join("Cargo.toml")).unwrap();
            let lock = std::fs::read_to_string(root.join(name).join("Cargo.lock")).unwrap();
            assert!(manifest_pin(name, &manifest).is_ok());
            assert!(lock_pin(name, &lock).is_ok());
            for bad in [
                format!("{manifest}\n[build-dependencies]\ncc = \"1\"\n"),
                manifest.replace("[dependencies]", "[target.'cfg(unix)'.dependencies]"),
                manifest.replace("[dependencies]", "[dependencies]\nring = \"0.17\""),
            ] {
                assert!(manifest_pin(name, &bad).is_err());
            }
            for bad in [
                lock.replace("version = \"0.45.0\"", "version = \"0.44.0\""),
                lock.replace("checksum = \"", "checksum = \"0"),
                format!("{lock}\n[[package]]\nname = \"unapproved\"\nversion = \"1.0.0\"\n"),
            ] {
                assert!(lock_pin(name, &bad).is_err());
            }
            assert!(manifest_pin("td-other", &manifest).is_err());
            assert!(lock_pin("td-other", &lock).is_err());
        }
    }

    #[test]
    fn active_graph_rejects_inactive_backend_and_feature_drift() {
        let root = root();
        if !root.join("td-crypto").is_dir() {
            return;
        }
        let crypto = root.join("td-crypto").canonicalize().unwrap();
        let graph = format!("{ACTIVE}td-crypto v0.1.0 ({})|\n", crypto.display());
        assert!(active_graph(&root, "td-crypto", &graph).is_ok());
        for bad in [
            format!("{graph}ring v0.17.14|alloc\n"),
            graph.replace("aws-lc-sys v0.45.0", "aws-lc-sys v0.44.0"),
            graph.replace(
                "|aws-lc-rs,aws_lc_rs,std,tls12",
                "|aws-lc-rs,aws_lc_rs,ring,std,tls12",
            ),
            graph.replace("webpki-roots v1.0.8|\n", ""),
            graph.replace(&crypto.display().to_string(), "/wrong/source"),
        ] {
            assert!(active_graph(&root, "td-crypto", &bad).is_err());
        }
        let mail = root.join("td-mta").canonicalize().unwrap();
        let mailgraph = format!("{graph}td-mta v0.1.0 ({})|\n", mail.display());
        assert!(active_graph(&root, "td-mta", &mailgraph).is_ok());
        assert!(active_graph(&root, "td-mta", &graph).is_err());
    }
}
