//! Host-only, pinned inputs for the standalone musl artifact.

use crate::crypto_headers::Architecture;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

const HOST: &str = "x86_64-unknown-linux-gnu";
const DIST: &str = "https://static.rust-lang.org/dist/2026-10-15";

struct Archive {
    name: &'static str,
    component: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const RUST: &[Archive] = &[
    Archive {
        name: "rustc-1.99.1-x86_64-unknown-linux-gnu",
        component: "rustc",
        bytes: 84_098_960,
        sha256: "2ff05ff9c08a90861e7f6e4065b40a8b87463b4576186fd613871c3e37f2091f",
    },
    Archive {
        name: "cargo-1.99.1-x86_64-unknown-linux-gnu",
        component: "cargo",
        bytes: 10_954_816,
        sha256: "53f3521949404d595efb06a55c12c5658d4ed1c75566f33d3d514eaebe70bd8a",
    },
    Archive {
        name: "rust-std-1.99.1-x86_64-unknown-linux-gnu",
        component: "rust-std-x86_64-unknown-linux-gnu",
        bytes: 31_563_228,
        sha256: "109b074225bf5b9ea3820890876bfdf4ed161adbfaf94b2b4b5984c85ee5d958",
    },
    Archive {
        name: "rust-std-1.99.1-x86_64-unknown-linux-musl",
        component: "rust-std-x86_64-unknown-linux-musl",
        bytes: 41_325_128,
        sha256: "1dd241eaeb19ff3b5180461c77bdee93d6cafc669eda9de834e7c06b0ec3ca95",
    },
];

const ARM_RUST: &[Archive] = &[Archive {
    name: "rust-std-1.99.1-aarch64-unknown-linux-musl",
    component: "rust-std-aarch64-unknown-linux-musl",
    bytes: 31_162_764,
    sha256: "2f3eb0d0610e2b7f6b2121d106e2490ac648decfa0c09b30873bdfc60de6c1ab",
}];

fn rust_archives(architecture: Architecture) -> impl Iterator<Item = &'static Archive> {
    RUST.iter()
        .filter(move |pin| {
            architecture == Architecture::X86_64
                || pin.component != "rust-std-x86_64-unknown-linux-musl"
        })
        .chain(match architecture {
            Architecture::X86_64 => [].iter(),
            Architecture::Aarch64 => ARM_RUST.iter(),
        })
}

fn tree_hash(path: &Path) -> Result<String> {
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return Err(format!("input is not a real directory: {}", path.display()));
    }
    crate::sandbox::nar_hash_of(path).map_err(|e| format!("hash {}: {e}", path.display()))
}

fn cache_path(parent: &Path, label: &str, expected: &str) -> Result<PathBuf> {
    let digest = expected
        .strip_prefix("sha256:")
        .ok_or("unexpected tree hash form")?;
    Ok(parent.join(format!("crypto-{label}-{digest}")))
}

fn verified_cache(destination: &Path, expected: &str) -> Result<bool> {
    match fs::symlink_metadata(destination) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat portable cache: {e}")),
    }
    if tree_hash(destination)? != expected {
        return Err(format!(
            "portable cache differs from verified inputs: {}; remove this entry and retry",
            destination.display()
        ));
    }
    Ok(true)
}

pub(crate) fn publish_tree(
    source: &Path,
    parent: &Path,
    label: &str,
    expected: &str,
) -> Result<PathBuf> {
    let destination = cache_path(parent, label, expected)?;
    if verified_cache(&destination, expected)? {
        return Ok(destination);
    }
    if tree_hash(source)? != expected {
        return Err("prepared input changed before publication".into());
    }
    if let Err(e) = fs::rename(source, &destination) {
        if !verified_cache(&destination, expected)? {
            return Err(format!("publish portable input: {e}"));
        }
    }
    if !verified_cache(&destination, expected)? {
        return Err("published portable input disappeared".into());
    }
    Ok(destination)
}

/// The pinned Debian build root (AGENTS.md, "Debian build root"): the
/// portable compile's C toolchain and the host runtime its upstream Rust
/// tools and build scripts run on.
pub(crate) struct BuildRoot {
    /// The authenticated tree on disk.
    pub(crate) directory: PathBuf,
    /// Its store path and pinned NAR hash, for `BUILD-INPUTS`.
    pub(crate) record: String,
    /// The evaluator's ladder lock, held shared while the tree is bound, as
    /// a build holds it: `clear-store` cannot remove the tree under the
    /// kit's mounts.
    pub(crate) _lock: fs::File,
}

fn absolute_record(text: &str) -> Result<PathBuf> {
    let path = PathBuf::from(text);
    if !path.is_absolute()
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err("build root seed path must be absolute without parent traversal".into());
    }
    Ok(path)
}

/// The seed store, seed db and ladder lock `td-recipe-eval build-root-seed`
/// reports. Other lines (progress) are skipped; each record must appear once.
fn seed_record(text: &str) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let (mut store, mut db, mut lock) = (None, None, None);
    for line in text.lines() {
        let (slot, path) = if let Some(path) = line.strip_prefix("TD_BUILD_ROOT_STORE ") {
            (&mut store, path)
        } else if let Some(path) = line.strip_prefix("TD_BUILD_ROOT_SEED_DB ") {
            (&mut db, path)
        } else if let Some(path) = line.strip_prefix("TD_BUILD_ROOT_LOCK ") {
            (&mut lock, path)
        } else {
            continue;
        };
        if slot.replace(absolute_record(path)?).is_some() {
            return Err("duplicate build root seed record".into());
        }
    }
    Ok((
        store.ok_or("td-recipe-eval reported no build root seed store")?,
        db.ok_or("td-recipe-eval reported no build root seed db")?,
        lock.ok_or("td-recipe-eval reported no ladder lock")?,
    ))
}

/// The evaluator places the root in the machine's shared seed store, as a
/// build-root recipe's check does. Its answer only says where to look:
/// td-builder authenticates that tree against its own compiled lock,
/// re-hashing the bytes it will bind.
fn build_root(root: &Path) -> Result<BuildRoot> {
    let parent = root.join(".td-build-cache");
    let evaluator = crate::stage0::recipe_eval_place(root, &parent.join("recipe-eval"))?;
    let scratch = Scratch::new(&parent)?;
    let report = scratch.0.join("build-root-seed.out");
    let output =
        fs::File::create(&report).map_err(|e| format!("create build root seed report: {e}"))?;
    let mut command = Command::new(&evaluator);
    command
        .current_dir(root)
        .arg("build-root-seed")
        .stdout(output);
    crate::host_bin::arm_check_child(&mut command);
    let status = command
        .status()
        .map_err(|e| format!("place the portable build root: {e}"))?;
    if !status.success() {
        return Err(format!(
            "td-recipe-eval build-root-seed failed ({status}); a cold root needs `td-feed warm build-root'"
        ));
    }
    let text = crate::crypto_isolated::read_output(&report, "build-root-seed", 64 * 1024)?;
    let (store, seed_db, ladder) = seed_record(&text)?;
    // Taken before the tree is authenticated, so what is hashed is what stays
    // bound; a `clear-store` between the evaluator's exit and this only makes
    // the authentication below fail.
    let held = fs::File::open(&ladder)
        .map_err(|e| format!("open the ladder lock {}: {e}", ladder.display()))?;
    held.lock_shared()
        .map_err(|e| format!("lock the ladder {}: {e}", ladder.display()))?;
    let lock = crate::build_root::lock()?;
    let db = crate::build_root::db_path(&seed_db, &lock)?;
    let path = crate::build_root::authenticate_db(&db, &store)?;
    let (_, tree) = crate::build_root::pinned()?;
    let base = path
        .rsplit('/')
        .next()
        .filter(|base| !base.is_empty())
        .ok_or("build root store path has no basename")?;
    let directory = store.join(base);
    if !fs::symlink_metadata(&directory).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "build root is not a real directory: {}",
            directory.display()
        ));
    }
    Ok(BuildRoot {
        directory,
        record: format!("build-root {path} {tree}\n"),
        _lock: held,
    })
}

struct Scratch(PathBuf);
impl Scratch {
    fn new(parent: &Path) -> Result<Self> {
        fs::create_dir_all(parent).map_err(|e| format!("portable input parent: {e}"))?;
        for attempt in 0..128 {
            let path = parent.join(format!("crypto-portable-{}-{attempt}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("portable scratch: {e}")),
            }
        }
        Err("portable scratch names exhausted".into())
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_archive(source: &Path, destination: &Path, pin: &Archive) -> Result<()> {
    if !fs::symlink_metadata(source).is_ok_and(|m| m.is_file()) {
        return Err(format!(
            "pinned archive is not a regular file: {}",
            source.display()
        ));
    }
    let input = fs::File::open(source)
        .map_err(|e| format!("open pinned archive {}: {e}", source.display()))?;
    let metadata = input.metadata().map_err(|e| format!("stat archive: {e}"))?;
    if !metadata.is_file() || metadata.len() != pin.bytes {
        return Err(format!(
            "{} must be a {}-byte regular archive",
            source.display(),
            pin.bytes
        ));
    }
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|e| format!("create private archive: {e}"))?;
    let mut input = input.take(pin.bytes + 1);
    let mut hash = crate::sha256::Sha256::new();
    let mut size = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|e| format!("read archive: {e}"))?;
        if count == 0 {
            break;
        }
        let bytes = buffer.get(..count).ok_or("archive read exceeds buffer")?;
        size += count as u64;
        if size > pin.bytes {
            return Err("archive grew past its pinned length".into());
        }
        hash.update(bytes);
        output
            .write_all(bytes)
            .map_err(|e| format!("write private archive: {e}"))?;
    }
    if size != pin.bytes || crate::sha256::to_base16(&hash.finalize()) != pin.sha256 {
        return Err(format!(
            "{} has the wrong pinned size or SHA-256",
            source.display()
        ));
    }
    output
        .flush()
        .map_err(|e| format!("flush private archive: {e}"))
}

fn rust_receipt(architecture: Architecture) -> String {
    let target = architecture.triple();
    let mut text = format!("portable-inputs 1\nhost {HOST}\ntarget {target}\n");
    for pin in rust_archives(architecture) {
        text.push_str(&format!(
            "archive {DIST}/{}.tar.xz {} {}\n",
            pin.name, pin.bytes, pin.sha256
        ));
    }
    text
}

fn check_overlay(source: &Path, destination: &Path, depth: usize) -> Result<()> {
    if depth > 64 {
        return Err("Rust component nesting exceeds 64 directories".into());
    }
    if let Ok(meta) = fs::symlink_metadata(destination) {
        if !meta.is_dir() {
            return Err("Rust component directory clashes with an existing entry".into());
        }
    }
    for entry in fs::read_dir(source).map_err(|e| format!("read Rust component: {e}"))? {
        let entry = entry.map_err(|e| format!("Rust component entry: {e}"))?;
        let destination = destination.join(entry.file_name());
        let kind = entry
            .file_type()
            .map_err(|e| format!("Rust component type: {e}"))?;
        if kind.is_dir() {
            check_overlay(&entry.path(), &destination, depth + 1)?;
        } else if kind.is_file() || kind.is_symlink() {
            match fs::symlink_metadata(&destination) {
                Ok(_) => {
                    return Err(format!(
                        "Rust component file clash: {}",
                        destination.display()
                    ))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("inspect Rust destination: {e}")),
            }
        } else {
            return Err("unsupported Rust component entry type".into());
        }
    }
    Ok(())
}

fn install_component(package: &Path, pin: &Archive, kit: &Path) -> Result<()> {
    // Install the reviewed component as data, never by executing install.sh.
    for entry in fs::read_dir(package.join(pin.component))
        .map_err(|e| format!("read Rust component: {e}"))?
    {
        let entry = entry.map_err(|e| format!("Rust component entry: {e}"))?;
        let name = entry.file_name();
        if name == "manifest.in" {
            continue;
        }
        if !entry
            .file_type()
            .map_err(|e| format!("stat Rust component: {e}"))?
            .is_dir()
        {
            return Err(format!(
                "unexpected Rust component top-level entry: {}",
                entry.path().display()
            ));
        }
        let destination = kit.join(name);
        check_overlay(&entry.path(), &destination, 0)?;
        crate::build::copy_tree_writable(&entry.path(), &destination)?;
    }
    let notices = kit.join("portable-notices").join(pin.name);
    fs::create_dir_all(&notices).map_err(|e| format!("create Rust notice directory: {e}"))?;
    for name in [
        "COPYRIGHT",
        "LICENSE-APACHE",
        "LICENSE-MIT",
        "LICENSE-THIRD-PARTY",
    ] {
        let source = package.join(name);
        let required = matches!(name, "LICENSE-APACHE" | "LICENSE-MIT")
            || (pin.component == "cargo" && name == "LICENSE-THIRD-PARTY")
            || (pin.component != "cargo" && name == "COPYRIGHT");
        if required || source.exists() {
            fs::copy(source, notices.join(name))
                .map_err(|e| format!("copy Rust notice {name}: {e}"))?;
        }
    }
    Ok(())
}

fn prepare_rust(root: &Path, archives: &Path, architecture: Architecture) -> Result<PathBuf> {
    let parent = root.join(".td-build-cache");
    let scratch = Scratch::new(&parent)?;
    let kit = scratch.0.join("kit");
    for pin in rust_archives(architecture) {
        let filename = format!("{}.tar.xz", pin.name);
        let private = scratch.0.join(&filename);
        copy_archive(&archives.join(filename), &private, pin)?;
        let unpacked = scratch.0.join(pin.name);
        crate::tar::extract_tar_xz(&private, &unpacked)?;
        let package = unpacked.join(pin.name);
        install_component(&package, pin, &kit)?;
        fs::remove_dir_all(&unpacked).map_err(|e| format!("remove extracted Rust input: {e}"))?;
        fs::remove_file(&private).map_err(|e| format!("remove private Rust archive: {e}"))?;
    }
    fs::write(kit.join("PORTABLE-INPUTS"), rust_receipt(architecture))
        .map_err(|e| format!("write portable Rust receipt: {e}"))?;
    let expected = tree_hash(&kit)?;
    publish_tree(&kit, &parent, "rust-1.99.1", &expected)
}

pub(crate) struct Inputs {
    pub(crate) rust: PathBuf,
    pub(crate) headers: PathBuf,
    pub(crate) build_root: BuildRoot,
}

pub(crate) fn inputs_architecture(
    root: &Path,
    archives: &Path,
    architecture: Architecture,
) -> Result<Inputs> {
    Ok(Inputs {
        rust: prepare_rust(root, archives, architecture)?,
        headers: crate::crypto_headers::prepare_architecture(
            root,
            &archives.join("musl-1.2.5.tar.gz"),
            architecture,
        )?,
        build_root: build_root(root)?,
    })
}

pub(crate) fn prepare(root: &Path, archives: &Path) -> Result<()> {
    prepare_architecture(root, archives, Architecture::X86_64)
}

pub(crate) fn prepare_architecture(
    root: &Path,
    archives: &Path,
    architecture: Architecture,
) -> Result<()> {
    let inputs = inputs_architecture(root, archives, architecture)?;
    println!("portable-rust {}", inputs.rust.display());
    println!("portable-musl-headers {}", inputs.headers.display());
    print!("{}", inputs.build_root.record);
    println!(
        "portable-build-root {}",
        inputs.build_root.directory.display()
    );
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn target_manifests_select_the_matching_standard_library() {
        for architecture in [Architecture::X86_64, Architecture::Aarch64] {
            let pins: Vec<_> = rust_archives(architecture).collect();
            assert_eq!(pins.len(), 4);
            assert_eq!(
                pins.iter().filter(|pin| pin.component == "rustc").count(),
                1
            );
            let component = format!("rust-std-{}", architecture.triple());
            assert_eq!(
                pins.iter().filter(|pin| pin.component == component).count(),
                1
            );
            let receipt = rust_receipt(architecture);
            assert!(receipt.contains(&format!("target {}\n", architecture.triple())));
        }
        assert!(!rust_receipt(Architecture::Aarch64)
            .contains("rust-std-1.99.1-x86_64-unknown-linux-musl"));
        assert!(!rust_receipt(Architecture::X86_64).contains("aarch64"));
    }

    #[test]
    fn private_archive_bytes_must_match_the_pin() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let source = scratch.0.join("archive");
        let pin = Archive {
            name: "fixture",
            component: "fixture",
            bytes: 3,
            sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        };
        fs::write(&source, b"abc").unwrap();
        let private = scratch.0.join("private");
        copy_archive(&source, &private, &pin).unwrap();
        fs::write(&source, b"bad").unwrap();
        assert_eq!(fs::read(&private).unwrap(), b"abc");
        assert!(copy_archive(&source, &scratch.0.join("bad"), &pin)
            .unwrap_err()
            .contains("SHA-256"));
        fs::write(&source, b"abcd").unwrap();
        assert!(copy_archive(&source, &scratch.0.join("long"), &pin).is_err());
        fs::remove_file(&source).unwrap();
        assert!(copy_archive(&source, &scratch.0.join("missing"), &pin).is_err());
        symlink(&private, &source).unwrap();
        assert!(copy_archive(&source, &scratch.0.join("link"), &pin).is_err());
        assert!(copy_archive(&private, &private, &pin).is_err());
    }

    #[test]
    fn publication_reuses_exact_trees_and_rejects_tampering() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let parent = scratch.0.join("cache");
        let source = scratch.0.join("source");
        fs::create_dir(&parent).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"data").unwrap();
        let expected = tree_hash(&source).unwrap();
        let published = publish_tree(&source, &parent, "fixture", &expected).unwrap();
        assert_eq!(
            publish_tree(&source, &parent, "fixture", &expected).unwrap(),
            published
        );
        fs::write(published.join("file"), b"edit").unwrap();
        assert!(publish_tree(&source, &parent, "fixture", &expected).is_err());
        fs::write(published.join("file"), b"data").unwrap();
        fs::set_permissions(published.join("file"), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(publish_tree(&source, &parent, "fixture", &expected).is_err());
        fs::remove_dir_all(&published).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file"), b"data").unwrap();
        symlink(&source, &published).unwrap();
        assert!(publish_tree(&source, &parent, "fixture", &expected).is_err());
    }

    fn package(root: &Path, pin: &Archive) -> PathBuf {
        let path = root.join(pin.name);
        fs::create_dir_all(path.join(pin.component).join("bin")).unwrap();
        fs::write(
            path.join(pin.component).join("manifest.in"),
            b"not installed",
        )
        .unwrap();
        fs::write(path.join(pin.component).join("bin/tool"), b"binary").unwrap();
        fs::set_permissions(
            path.join(pin.component).join("bin/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for name in [
            "COPYRIGHT",
            "LICENSE-APACHE",
            "LICENSE-MIT",
            "LICENSE-THIRD-PARTY",
        ] {
            fs::write(path.join(name), name).unwrap();
        }
        path
    }

    #[test]
    fn rust_components_keep_notices_and_refuse_clashes_and_unexpected_entries() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let pin = RUST.get(1).unwrap();
        let input = package(&scratch.0, pin);
        let kit = scratch.0.join("kit");
        install_component(&input, pin, &kit).unwrap();
        assert_eq!(fs::read(kit.join("bin/tool")).unwrap(), b"binary");
        assert_ne!(
            fs::metadata(kit.join("bin/tool"))
                .unwrap()
                .permissions()
                .mode()
                & 0o100,
            0
        );
        assert!(!kit.join("manifest.in").exists());
        assert_eq!(
            fs::read(
                kit.join("portable-notices")
                    .join(pin.name)
                    .join("LICENSE-THIRD-PARTY")
            )
            .unwrap(),
            b"LICENSE-THIRD-PARTY"
        );
        assert!(install_component(&input, pin, &kit)
            .unwrap_err()
            .contains("clash"));
        for (index, notice) in ["LICENSE-APACHE", "LICENSE-MIT", "LICENSE-THIRD-PARTY"]
            .iter()
            .enumerate()
        {
            fs::remove_file(input.join(notice)).unwrap();
            assert!(
                install_component(&input, pin, &scratch.0.join(format!("missing-{index}")))
                    .unwrap_err()
                    .contains("notice")
            );
            fs::write(input.join(notice), notice).unwrap();
        }
        let unexpected = input.join(pin.component).join("unexpected");
        fs::write(&unexpected, b"unexpected").unwrap();
        assert!(
            install_component(&input, pin, &scratch.0.join("file-entry"))
                .unwrap_err()
                .contains("top-level")
        );
        fs::remove_file(&unexpected).unwrap();
        symlink(input.join(pin.component).join("bin"), &unexpected).unwrap();
        assert!(
            install_component(&input, pin, &scratch.0.join("link-entry"))
                .unwrap_err()
                .contains("top-level")
        );
        let compiler = RUST.first().unwrap();
        let input = package(&scratch.0, compiler);
        fs::remove_file(input.join("COPYRIGHT")).unwrap();
        assert!(
            install_component(&input, compiler, &scratch.0.join("compiler"))
                .unwrap_err()
                .contains("COPYRIGHT")
        );
    }

    #[test]
    fn build_root_seed_records_are_exact() {
        let good = "progress\nTD_BUILD_ROOT_STORE /a/seed store\n\
            TD_BUILD_ROOT_SEED_DB /a/seed-db/x.db\nTD_BUILD_ROOT_LOCK /a.lock\n";
        assert_eq!(
            seed_record(good).unwrap(),
            (
                PathBuf::from("/a/seed store"),
                PathBuf::from("/a/seed-db/x.db"),
                PathBuf::from("/a.lock")
            )
        );
        let pair = "TD_BUILD_ROOT_STORE /a\nTD_BUILD_ROOT_SEED_DB /a.db\n";
        for bad in [
            format!("{good}TD_BUILD_ROOT_STORE /b\n"),
            format!("{good}TD_BUILD_ROOT_SEED_DB /b.db\n"),
            format!("{good}TD_BUILD_ROOT_LOCK /b.lock\n"),
            pair.into(),
            format!("{pair}TD_BUILD_ROOT_LOCK relative.lock\n"),
            "TD_BUILD_ROOT_STORE /a\nTD_BUILD_ROOT_LOCK /a.lock\n".into(),
            "TD_BUILD_ROOT_STORE relative\nTD_BUILD_ROOT_SEED_DB /a.db\nTD_BUILD_ROOT_LOCK /l\n"
                .into(),
            "TD_BUILD_ROOT_STORE /a/../b\nTD_BUILD_ROOT_SEED_DB /a.db\nTD_BUILD_ROOT_LOCK /l\n"
                .into(),
            String::new(),
        ] {
            assert!(seed_record(&bad).is_err(), "{bad}");
        }
    }
}
