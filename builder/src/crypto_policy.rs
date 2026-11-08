//! Named dependency admission for the crypto boundary: the mail crates and
//! td-tls-provider, the shared rustls provider.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

pub(crate) const LOCAL_SOURCES: &[&str] = &[
    "td-civil",
    "td-crypto",
    "td-fs",
    "td-header",
    "td-html",
    "td-json",
    "td-mime",
    "td-mta",
    "td-nfc",
    "td-primitives",
    "td-tls-provider",
];

pub(crate) const DEPENDENCIES: &[&str] = &[
    "aws-lc-rs = { version = \"=1.19.0\", default-features = false, features = [\"alloc\", \"non-fips\"] }",
    "rustls = { version = \"=0.23.46\", default-features = false, features = [\"std\", \"tls12\", \"aws_lc_rs\"] }",
    "webpki-roots = \"=1.0.11\"",
];

pub(crate) fn admitted(name: &str) -> bool {
    matches!(name, "td-crypto" | "td-mta" | "td-tls-provider")
}

pub(crate) fn manifest_pin(name: &str, text: &str) -> Result<(), String> {
    let expected = match name {
        "td-civil" => "434e8c6ab3f8d304ce8c090478324e4e1398e39561395b0c7a59bf4cdc20b1ca",
        "td-crypto" => "d838d471f755e194e2001229e67b04c8281b46b7cd134737b7c2b65077ef7aec",
        "td-mta" => "24f2b491cd3b9be70711ffdcd47b19baf6615a2637a411e3b1abc8235be41f6b",
        "td-fs" => "e685546b8dc4353148f58034fc27b8bd59490281944f9226f295b5d6d72240fc",
        "td-header" => "4e8dd9a6be096e9ffa65cbb26e71a8f3ec8a9c32c9d83211a1c490a43508aac9",
        "td-html" => "e4c924efdc7deab65fcc5cfa64bd5349935d5f3094e4310125ddb173af0b1c14",
        "td-json" => "2793cd9cd8ffc7bac436069324b42b503f7f3114418fc95f558fb0831060e8b3",
        "td-nfc" => "39d752d381e239345e4ba213ea2ce2fb4efb723a4ea9f16f1d8aa1f1173359cf",
        "td-mime" => "3f14e32d7ee4f4b11e57845153df490fc4d0480f282477ecebf2349d5347b1d4",
        "td-primitives" => "056b2c47072b8e101730f86e33fbe79a00917aaa91a12aa17a7a801070bd4b85",
        "td-tls-provider" => "b6c4742d6c92c053f6d1595dbb29ff11a8396775e23e30e0317bb6ee72f10298",
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
        "td-civil" => "d05684e2af245c0a796e154148ced1bc85ea08d9f31799d65a4fe1a62b7fc66a",
        "td-crypto" => "101fd34f66e0b38c153d6e8d11670f5cd757963fa50feb90336d3ebb71f8f92d",
        "td-mta" => "26d27bb88497f325ac8935f0ae7cb3902fcafea1234766806dabf24051a2925f",
        "td-fs" => "d907e7d97198344c058b8403d2a07c08174081bcd8af0f4d9e276c0131766fe2",
        "td-header" => "2862fd9186d5cdef3d645af0b43dee9f77ba51beee5d98219e5aae30db11eabc",
        "td-html" => "5bc92c6117fc17c612004a1b025a6ad42171c3d185d9af7eb47dd13a9168e8e3",
        "td-json" => "679f89cdafa0f8457884ba0d0f0c197814d2b13e4f6ece4557575c9bdfe3848c",
        "td-nfc" => "6b447fb42d5b4e2aa2a02da0817647db6f178d445f8dc3861c7c9a011cce4695",
        "td-mime" => "72893a76d9ecfaad70a4bcf8447386fa5a0d2127ec5c4c770c9ac14438c0a7ed",
        "td-primitives" => "388616b0ea8bc440730c90424f57a66dd8921b6c6b08ab2518d51d6da561fd13",
        "td-tls-provider" => "46e710a0aaab04b17090c56ce3c7dcb46de35c18b4016ef2091f9969328cb389",
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
            "{name}/{file} differs from the reviewed mail/crypto source pin; review the manifest, lock and feature closure before updating this policy"
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
const ACTIVE: &str = "aws-lc-rs v1.19.0|alloc,aws-lc-sys,non-fips,prebuilt-nasm
aws-lc-sys v0.47.0|prebuilt-nasm
cc v1.11.0|parallel
cmake v0.1.58|
dunce v1.0.5|
find-msvc-tools v0.1.15|
fs_extra v1.3.0|
jobserver v0.1.35|
libc v0.2.191|default,std
once_cell v1.21.4|alloc,race,std
pkg-config v0.3.34|
rustls v0.23.46|aws-lc-rs,aws_lc_rs,std,tls12
rustls-pki-types v1.15.1|alloc,default,std
rustls-webpki v0.103.16|alloc,aws-lc-rs,std
shlex v2.0.1|default,std
subtle v2.6.1|
untrusted v0.9.0|
webpki-roots v1.0.11|
zeroize v1.9.1|alloc,default
";

const POSTGRES_ACTIVE: &str = "async-trait v0.1.92 (proc-macro)|
base64 v0.22.1|alloc,default,std
block-buffer v0.12.1|
byteorder v1.5.0|default,std
bytes v1.12.1|default,std
cfg-if v1.0.5|
chacha20 v0.10.2|rng
cmov v0.5.4|
const-oid v0.10.2|
cpufeatures v0.3.1|
crypto-common v0.2.2|
ctutils v0.4.8|
digest v0.11.3|alloc,block-api,default,mac,oid
fallible-iterator v0.2.0|default,std
futures-channel v0.3.34|alloc,default,futures-sink,sink,std
futures-core v0.3.34|alloc,default,std
futures-sink v0.3.34|alloc,default,std
futures-task v0.3.34|
futures-util v0.3.34|futures-sink,sink
getrandom v0.4.3|std,sys_rng
hmac v0.13.0|
hybrid-array v0.4.15|
lock_api v0.4.14|atomic_usize,default
log v0.4.35|
md-5 v0.11.0|alloc,default,oid
memchr v2.8.3|alloc,default,std
parking_lot v0.12.5|default
parking_lot_core v0.9.12|
percent-encoding v2.3.2|alloc,default,std
phf v0.13.1|default,std
phf_shared v0.13.1|std
pin-project-lite v0.2.17|
postgres-protocol v0.6.12|default
postgres-types v0.2.14|
proc-macro2 v1.0.107|default,proc-macro
quote v1.0.47|default,proc-macro
rand v0.10.3|alloc,default,std,std_rng,sys_rng,thread_rng
rand_core v0.10.1|
scopeguard v1.2.0|
sha2 v0.11.1|alloc,default,oid
siphasher v1.0.4|default,std
smallvec v1.16.4|
socket2 v0.6.5|all
stringprep v0.1.5|
syn v3.0.9|clone-impls,full,parsing,printing,proc-macro,visit-mut
tinyvec v1.13.3|alloc,default
tokio v1.54.1|bytes,default,io-util,sync
tokio-postgres v0.7.18|
tokio-util v0.7.20|codec,default,libc
typenum v1.20.1|const-generics
unicode-bidi v0.3.18|default,hardcoded-data,std
unicode-ident v1.0.27|
unicode-normalization v0.1.26|default,std
unicode-properties v0.1.5|default,emoji,general-category
whoami v2.1.3|default,std,wasi-wasite,wasm-web
";

/// td-tls-provider's selected graph: rustls with no built-in backend, its
/// cryptography all td-primitives, which is std only.
const TLS_PROVIDER_ACTIVE: &str = "once_cell v1.21.4|alloc,race,std
rustls v0.23.46|custom-provider,std,tls12
rustls-pki-types v1.15.1|alloc,default,std
rustls-webpki v0.103.16|alloc,std
subtle v2.6.1|
untrusted v0.9.0|
zeroize v1.9.1|alloc,default
";

/// Vendored directory names (`name-version`) of the packages the admitted
/// graphs compile. Locked-only packages contribute no code to an artifact.
pub(crate) fn active_vendor_dirs() -> BTreeSet<String> {
    ACTIVE
        .lines()
        .chain(POSTGRES_ACTIVE.lines())
        .chain(TLS_PROVIDER_ACTIVE.lines())
        .filter_map(|line| {
            let (name, rest) = line.split_once(" v")?;
            let version = rest.split(['|', ' ']).next()?;
            Some(format!("{name}-{version}"))
        })
        .collect()
}

pub(crate) fn active_graph(root: &Path, name: &str, output: &str) -> Result<(), String> {
    if !admitted(name) {
        return Err(format!(
            "{name} has no external crypto dependency admission"
        ));
    }
    let (mut expected, mut local): (BTreeSet<String>, _) = if name == "td-tls-provider" {
        (
            TLS_PROVIDER_ACTIVE.lines().map(str::to_owned).collect(),
            vec!["td-primitives", "td-tls-provider"],
        )
    } else {
        (
            ACTIVE.lines().map(str::to_owned).collect(),
            vec!["td-crypto"],
        )
    };
    if name == "td-mta" {
        expected.extend(POSTGRES_ACTIVE.lines().map(str::to_owned));
        local.extend([
            "td-civil",
            "td-fs",
            "td-header",
            "td-html",
            "td-json",
            "td-mime",
            "td-mta",
            "td-nfc",
        ]);
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

pub(crate) const POSTGRES_DEPENDENCIES: &[&str] = &[
    "tokio-postgres = { version = \"=0.7.18\", default-features = false }",
    "tokio = { version = \"=1.54.1\", default-features = false, features = [\"io-util\"] }",
];

/// td-tls-provider's one external line: rustls's engine and nothing else, its
/// built-in backends off.
pub(crate) const TLS_PROVIDER_DEPENDENCIES: &[&str] = &[
    "rustls = { version = \"=0.23.46\", default-features = false, features = [\"custom-provider\", \"std\", \"tls12\"] }",
];

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
        let _base_cleanup = crate::test_temp::TempDir::new("td-crypto-config").unwrap();
        let base = _base_cleanup.join("fixture");
        std::fs::create_dir(&base).unwrap();
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
            "[target.x86_64-unknown-linux-gnu.aws_lc_0_47_0]\nrustc-link-lib = [\"crypto\"]\n",
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
            assert!(error.contains("reviewed mail/crypto source pin"), "{error}");
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
        for name in LOCAL_SOURCES {
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
    fn std_source_pins_refuse_dependency_changes() {
        let root = root();
        for name in [
            "td-civil",
            "td-fs",
            "td-header",
            "td-html",
            "td-json",
            "td-mime",
            "td-nfc",
            "td-primitives",
        ] {
            if !root.join(name).join("Cargo.toml").exists() {
                continue;
            }
            let manifest = std::fs::read_to_string(root.join(name).join("Cargo.toml")).unwrap();
            let lock = std::fs::read_to_string(root.join(name).join("Cargo.lock")).unwrap();
            assert!(manifest_pin(name, &manifest).is_ok());
            assert!(lock_pin(name, &lock).is_ok());
            assert!(manifest_pin(
                name,
                &format!("{manifest}\n[dependencies]\nforeign = \"1\"\n")
            )
            .is_err());
            assert!(lock_pin(
                name,
                &format!("{lock}\n[[package]]\nname = \"foreign\"\nversion = \"1.0.0\"\n")
            )
            .is_err());
            assert!(!admitted(name));
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
                lock.replace("version = \"0.47.0\"", "version = \"0.46.0\""),
                lock.replace("checksum = \"", "checksum = \"0"),
                format!("{lock}\n[[package]]\nname = \"unapproved\"\nversion = \"1.0.0\"\n"),
            ] {
                assert!(lock_pin(name, &bad).is_err());
            }
            assert!(manifest_pin("td-other", &manifest).is_err());
            assert!(lock_pin("td-other", &lock).is_err());
        }
        let manifest = std::fs::read_to_string(root.join("td-tls-provider/Cargo.toml")).unwrap();
        let lock = std::fs::read_to_string(root.join("td-tls-provider/Cargo.lock")).unwrap();
        assert!(manifest_pin("td-tls-provider", &manifest).is_ok());
        assert!(lock_pin("td-tls-provider", &lock).is_ok());
        for line in TLS_PROVIDER_DEPENDENCIES {
            assert!(manifest.lines().any(|candidate| candidate == *line));
        }
        // The hash pins are what refuse these: any byte changed reds, a
        // backend feature or extra dependency among them.
        for bad in [
            format!("{manifest}\n[build-dependencies]\ncc = \"1\"\n"),
            manifest.replace("\"custom-provider\", ", ""),
            manifest.replace("\"tls12\"]", "\"tls12\", \"ring\"]"),
            manifest.replace("[dependencies]", "[dependencies]\nlog = \"0.4\""),
        ] {
            assert!(manifest_pin("td-tls-provider", &bad).is_err());
        }
        for bad in [
            lock.replace("version = \"0.23.46\"", "version = \"0.23.45\""),
            lock.replace("checksum = \"", "checksum = \"0"),
            format!("{lock}\n[[package]]\nname = \"unapproved\"\nversion = \"1.0.0\"\n"),
        ] {
            assert!(lock_pin("td-tls-provider", &bad).is_err());
        }
    }

    #[test]
    fn active_vendor_dirs_name_every_active_package_and_no_locked_only_one() {
        let dirs = active_vendor_dirs();
        // The graphs share packages: td-tls-provider's rustls closure is the
        // crypto graph's under other features.
        let packages: BTreeSet<&str> = ACTIVE
            .lines()
            .chain(POSTGRES_ACTIVE.lines())
            .chain(TLS_PROVIDER_ACTIVE.lines())
            .filter_map(|line| line.split_once('|').map(|(package, _)| package))
            .collect();
        assert_eq!(dirs.len(), packages.len());
        assert!(dirs.contains("aws-lc-rs-1.19.0"));
        assert!(dirs.contains("async-trait-0.1.92"));
        assert!(dirs.contains("whoami-2.1.3"));
        assert!(!dirs.contains("objc2-core-foundation-0.3.2"));
        assert!(!dirs.contains("wasite-1.0.2"));
        // A name or version that drifted from the lock would match no vendored
        // directory, and its notice would silently stop being required.
        let lock = std::fs::read_to_string(root().join("td-mta/Cargo.lock")).unwrap();
        let mut locked = BTreeSet::new();
        let mut name = None;
        for line in lock.lines() {
            if let Some(value) = line.strip_prefix("name = ") {
                name = Some(value.trim_matches('"').to_string());
            } else if let (Some(value), Some(name)) = (line.strip_prefix("version = "), &name) {
                locked.insert(format!("{name}-{}", value.trim_matches('"')));
            }
        }
        let missing: Vec<_> = dirs.difference(&locked).collect();
        assert!(missing.is_empty(), "not in td-mta/Cargo.lock: {missing:?}");
    }

    /// Every registry package (name, version, checksum) of a lock.
    fn registry_packages(lock: &str) -> BTreeSet<(String, String, String)> {
        lock.split("[[package]]")
            .filter_map(|entry| {
                let field = |key: &str| {
                    entry.lines().find_map(|line| {
                        line.strip_prefix(key)
                            .map(|value| value.trim_matches('"').to_string())
                    })
                };
                Some((
                    field("name = ")?,
                    field("version = ")?,
                    field("checksum = ")?,
                ))
            })
            .collect()
    }

    /// The wrapper builds td-tls-provider from the common offline vendor,
    /// which is td-mta's lock extracted: a package the provider's lock
    /// holds and the mail lock does not would red only in the leg.
    #[test]
    fn tls_provider_lock_is_served_by_the_mail_vendor() {
        let root = root();
        let Ok(provider) = std::fs::read_to_string(root.join("td-tls-provider/Cargo.lock")) else {
            eprintln!("SKIP: builder-only source tree has no td-tls-provider lock");
            return;
        };
        let mail = std::fs::read_to_string(root.join("td-mta/Cargo.lock")).unwrap();
        let provider = registry_packages(&provider);
        let mail = registry_packages(&mail);
        assert!(provider.iter().any(|(name, ..)| name == "rustls"));
        let missing: Vec<_> = provider.difference(&mail).collect();
        assert!(missing.is_empty(), "not in td-mta/Cargo.lock: {missing:?}");
        let mut drifted = provider.clone();
        if let Some((name, version, checksum)) = drifted.pop_first() {
            drifted.insert((name, version, format!("0{checksum}")));
        }
        assert!(!drifted.is_subset(&mail));
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
            graph.replace("aws-lc-sys v0.47.0", "aws-lc-sys v0.46.0"),
            graph.replace(
                "|aws-lc-rs,aws_lc_rs,std,tls12",
                "|aws-lc-rs,aws_lc_rs,ring,std,tls12",
            ),
            graph.replace("webpki-roots v1.0.11|\n", ""),
            graph.replace(&crypto.display().to_string(), "/wrong/source"),
        ] {
            assert!(active_graph(&root, "td-crypto", &bad).is_err());
        }
        let mail = root.join("td-mta").canonicalize().unwrap();
        let civil = root.join("td-civil").canonicalize().unwrap();
        let json = root.join("td-json").canonicalize().unwrap();
        let header = root.join("td-header").canonicalize().unwrap();
        let html = root.join("td-html").canonicalize().unwrap();
        let nfc = root.join("td-nfc").canonicalize().unwrap();
        let mime = root.join("td-mime").canonicalize().unwrap();
        let fs = root.join("td-fs").canonicalize().unwrap();
        let mailgraph = format!(
            "{graph}{POSTGRES_ACTIVE}td-civil v0.1.0 ({})|\ntd-mta v0.1.0 ({})|\ntd-json v0.1.0 ({})|\ntd-header v0.1.0 ({})|\ntd-html v0.1.0 ({})|\ntd-nfc v0.1.0 ({})|\ntd-mime v0.1.0 ({})|\ntd-fs v0.1.0 ({})|\n",
            civil.display(),
            mail.display(),
            json.display(),
            header.display(),
            html.display(),
            nfc.display(),
            mime.display(),
            fs.display()
        );
        assert!(active_graph(
            &root,
            "td-mta",
            &format!("{graph}td-mta v0.1.0 ({})|\n", mail.display())
        )
        .is_err());
        assert!(active_graph(&root, "td-mta", &mailgraph).is_ok());
        assert!(active_graph(&root, "td-crypto", &format!("{graph}{POSTGRES_ACTIVE}")).is_err());
        assert!(active_graph(&root, "td-other", &mailgraph).is_err());
        for bad in [
            mailgraph.replace("smallvec v1.16.4|\n", ""),
            mailgraph.replace("tokio-postgres v0.7.18|\n", ""),
            mailgraph.replace(
                "tokio-postgres v0.7.18|",
                "tokio-postgres v0.7.18|default,runtime",
            ),
            mailgraph.replace(
                "tokio v1.54.1|bytes,default,io-util,sync",
                "tokio v1.54.1|bytes,default,io-util,net,rt,sync,time",
            ),
            mailgraph.replace(
                "postgres-protocol v0.6.12|default",
                "postgres-protocol v0.6.11|default",
            ),
            format!("{mailgraph}postgres-native-tls v0.5.0|\n"),
        ] {
            assert!(active_graph(&root, "td-mta", &bad).is_err());
        }

        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&format!("td-header v0.1.0 ({})|\n", header.display()), "")
        )
        .is_err());
        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&header.display().to_string(), "/wrong/header")
        )
        .is_err());
        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&format!("td-nfc v0.1.0 ({})|\n", nfc.display()), "")
        )
        .is_err());
        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&nfc.display().to_string(), "/wrong/nfc")
        )
        .is_err());
        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&format!("td-mime v0.1.0 ({})|\n", mime.display()), "")
        )
        .is_err());
        assert!(active_graph(
            &root,
            "td-mta",
            &mailgraph.replace(&format!("td-civil v0.1.0 ({})|\n", civil.display()), "")
        )
        .is_err());
        for bad in [
            mailgraph.replace(&format!("td-html v0.1.0 ({})|\n", html.display()), ""),
            mailgraph.replace(&html.display().to_string(), "/wrong/html"),
            mailgraph.replace("td-html v0.1.0", "td-html v0.2.0"),
        ] {
            assert!(active_graph(&root, "td-mta", &bad).is_err());
        }
        assert!(active_graph(&root, "td-mta", &graph).is_err());

        let provider = root.join("td-tls-provider").canonicalize().unwrap();
        let primitives = root.join("td-primitives").canonicalize().unwrap();
        let tls = format!(
            "{TLS_PROVIDER_ACTIVE}td-tls-provider v0.1.0 ({})|\ntd-primitives v0.1.0 ({})|\n",
            provider.display(),
            primitives.display()
        );
        assert!(active_graph(&root, "td-tls-provider", &tls).is_ok());
        assert!(active_graph(&root, "td-crypto", &tls).is_err());
        assert!(active_graph(&root, "td-tls-provider", &graph).is_err());
        for bad in [
            format!("{tls}ring v0.17.14|alloc\n"),
            format!("{tls}aws-lc-rs v1.19.0|alloc,aws-lc-sys,non-fips,prebuilt-nasm\n"),
            format!("{tls}log v0.4.35|\n"),
            tls.replace(
                "rustls v0.23.46|custom-provider,std,tls12",
                "rustls v0.23.46|custom-provider,logging,std,tls12",
            ),
            tls.replace(
                "rustls-webpki v0.103.16|alloc,std",
                "rustls-webpki v0.103.16|alloc,ring,std",
            ),
            tls.replace(
                &format!("td-primitives v0.1.0 ({})|\n", primitives.display()),
                "",
            ),
            tls.replace(&primitives.display().to_string(), "/wrong/primitives"),
            format!("{tls}td-crypto v0.1.0 ({})|\n", crypto.display()),
        ] {
            assert!(active_graph(&root, "td-tls-provider", &bad).is_err());
        }
    }
}
