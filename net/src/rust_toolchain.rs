//! Pinned upstream Rust, prepared on first use in the caller's private cache.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use td_engine::{rust_dist, sha256};

const CONFIG: &str = "/etc/td-rust.tsv";
const MAX_CONFIG: u64 = 16 * 1024;
const MAX_ARCHIVE: u64 = 512 * 1024 * 1024;
const TOOLS: &[&str] = &["rustc", "rustdoc", "cargo", "cargo-clippy", "clippy-driver"];
const COMPONENTS: &[(&str, &str)] = &[
    ("rustc", "rustc/bin/rustc"),
    ("std", "rust-std-x86_64-unknown-linux-gnu/lib/rustlib"),
    ("cargo", "cargo/bin/cargo"),
    ("clippy", "clippy-preview/bin/clippy-driver"),
];
const LLD: &str = "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-lld";

struct Source {
    name: String,
    url: String,
    digest: String,
}
struct Config {
    identity: String,
    version: String,
    glibc: PathBuf,
    libgcc: PathBuf,
    libz: PathBuf,
    sources: Vec<Source>,
}

fn store_path(path: &str) -> Result<PathBuf, String> {
    let p = Path::new(path);
    if !p.starts_with("/td/store")
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        || path.contains(['\0', '\t'])
    {
        return Err("Rust runtime path must be an absolute td store path".into());
    }
    Ok(p.into())
}

impl Config {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "Rust configuration is not UTF-8")?;
        let lines: Vec<_> = text.lines().collect();
        let ["td-rust-v1", version, glibc, libgcc, libz, rows @ ..] = lines.as_slice() else {
            return Err("unsupported Rust configuration".into());
        };
        if rows.len() != COMPONENTS.len()
            || version.is_empty()
            || !version.bytes().all(|c| c.is_ascii_digit() || c == b'.')
        {
            return Err("invalid Rust version or component count".into());
        }
        let mut sources = Vec::new();
        for (row, (expected, _)) in rows.iter().zip(COMPONENTS) {
            let fields: Vec<_> = row.split('\t').collect();
            let [name, url, digest] = fields.as_slice() else {
                return Err("invalid Rust component row".into());
            };
            if name != expected
                || !url.starts_with("https://static.rust-lang.org/dist/")
                || url.contains(['\0', ' ', '\t', '#', '?', '@'])
                || digest.len() != 64
                || !digest
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err("invalid Rust component pin".into());
            }
            sources.push(Source {
                name: (*name).into(),
                url: (*url).into(),
                digest: (*digest).into(),
            });
        }
        Ok(Self {
            identity: sha256::hex_digest(bytes),
            version: (*version).into(),
            glibc: store_path(glibc)?,
            libgcc: store_path(libgcc)?,
            libz: store_path(libz)?,
            sources,
        })
    }
}

fn read_config() -> Result<Config, String> {
    let (file, meta) = crate::td_fs::open_real_file(Path::new(CONFIG), "Rust configuration")
        .map_err(|e| e.to_string())?;
    if meta.len() > MAX_CONFIG {
        return Err("Rust configuration is too large".into());
    }
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_CONFIG {
        return Err("Rust configuration grew while reading".into());
    }
    Config::parse(&bytes)
}

fn private_dir(path: &Path, uid: u32) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create {}: {e}", path.display())),
    }
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a private directory owned by this user",
            path.display()
        ));
    }
    Ok(())
}

fn cache_lock(root: &Path, uid: u32) -> Result<File, String> {
    let path = root.join("lock");
    let file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::td_fs::open_real_file(&path, "Rust cache lock")
                .map_err(|e| e.to_string())?
                .0
        }
        Err(e) => return Err(format!("create Rust cache lock: {e}")),
    };
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err("unsafe Rust cache lock".into());
    }
    file.lock().map_err(|e| format!("lock Rust cache: {e}"))?;
    Ok(file)
}

fn validate_tree(tree: &Path, identity: &str, uid: u32) -> Result<(), String> {
    private_dir(tree, uid)?;
    let (marker, meta) =
        crate::td_fs::open_real_file(&tree.join(".complete"), "Rust completion record")
            .map_err(|e| e.to_string())?;
    if meta.len() != identity.len() as u64 {
        return Err("incomplete Rust toolchain cache".into());
    }
    let mut value = String::new();
    marker
        .take(65)
        .read_to_string(&mut value)
        .map_err(|e| e.to_string())?;
    if value != identity {
        return Err("Rust toolchain cache identity mismatch".into());
    }
    for path in TOOLS
        .iter()
        .map(|name| format!("bin/{name}"))
        .chain(std::iter::once(LLD.into()))
    {
        let (mut file, meta) =
            crate::td_fs::open_real_file(&tree.join(path), "cached Rust executable")
                .map_err(|e| e.to_string())?;
        let mut magic = [0; 4];
        file.read_exact(&mut magic)
            .map_err(|e| format!("read Rust executable: {e}"))?;
        if magic != *b"\x7fELF" || meta.mode() & 0o111 == 0 {
            return Err("damaged Rust executable in cache".into());
        }
    }
    Ok(())
}

fn clear_work(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path).map_err(|e| e.to_string()),
        Ok(_) => Err("Rust cache staging path is not a real directory".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn sync_tree(path: &Path) -> Result<(), String> {
    for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            sync_tree(&entry.path())?;
        } else if kind.is_file() {
            File::open(entry.path())
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
    }
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

fn prepare(
    config: &Config,
    root: &Path,
    uid: u32,
    acquire: impl Fn(&str, &Path) -> Result<(), String>,
) -> Result<PathBuf, String> {
    private_dir(root, uid)?;
    let _lock = cache_lock(root, uid)?;
    let tree = root.join(&config.identity);
    match fs::symlink_metadata(&tree) {
        Ok(_) => {
            validate_tree(&tree, &config.identity, uid)?;
            return Ok(tree);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("stat Rust cache: {e}")),
    }
    let work = root.join(".work");
    clear_work(&work)?;
    private_dir(&work, uid)?;
    let result = (|| {
        eprintln!(
            "td-rust: downloading and preparing Rust {} (first use)",
            config.version
        );
        for source in &config.sources {
            let archive = work.join(format!("{}.tar.xz", source.name));
            acquire(&source.url, &archive)?;
            let size = fs::metadata(&archive).map_err(|e| e.to_string())?.len();
            if size > MAX_ARCHIVE
                || sha256::sha256_file(&archive).map_err(|e| e.to_string())? != source.digest
            {
                return Err(format!(
                    "Rust {} archive checksum or size mismatch",
                    source.name
                ));
            }
        }
        for (name, required) in COMPONENTS {
            rust_dist::unpack_rust_component(
                &work.join(format!("{name}.tar.xz")),
                &work.join(name),
                required,
            )?;
        }
        let output = work.join("toolchain");
        private_dir(&output, uid)?;
        rust_dist::assemble(&rust_dist::RustDistInputs {
            rustc_src: work.join("rustc"),
            std_src: work.join("std"),
            cargo_src: work.join("cargo"),
            clippy_src: work.join("clippy"),
            glibc: config.glibc.clone(),
            libgcc_s: config.libgcc.clone(),
            libz: config.libz.clone(),
            out: output.clone(),
        })?;
        let mut marker = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(output.join(".complete"))
            .map_err(|e| e.to_string())?;
        marker
            .write_all(config.identity.as_bytes())
            .and_then(|_| marker.sync_all())
            .map_err(|e| e.to_string())?;
        validate_tree(&output, &config.identity, uid)?;
        sync_tree(&output)?;
        fs::rename(&output, &tree).map_err(|e| format!("publish Rust toolchain: {e}"))?;
        File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(tree)
    })();
    let cleanup = clear_work(&work);
    let tree = result?;
    cleanup?;
    Ok(tree)
}

fn ensure() -> Result<PathBuf, String> {
    let config = read_config()?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unset")?;
    let uid = fs::metadata("/proc/self").map_err(|e| e.to_string())?.uid();
    if !home.is_absolute() || fs::metadata(&home).map_err(|e| e.to_string())?.uid() != uid {
        return Err("HOME must belong to the invoking user".into());
    }
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    prepare(&config, &home.join(".td-rust"), uid, |url, path| {
        crate::http::get_to_file_before(url, path, MAX_ARCHIVE, deadline)
    })
}

pub(crate) fn is_tool(name: &str) -> bool {
    TOOLS.contains(&name)
}

pub(crate) fn run(args: &[String]) {
    let result = (|| {
        let first = args.first().ok_or("missing Rust command")?;
        let name = Path::new(first)
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or("invalid Rust command")?;
        let (tool, rest) = if is_tool(name) {
            (name, args.get(1..).unwrap_or(&[]))
        } else {
            (
                args.get(1).map(String::as_str).unwrap_or("help"),
                args.get(2..).unwrap_or(&[]),
            )
        };
        if matches!(tool, "help" | "--help" | "-h") {
            println!("usage: td-rust fetch|path|rustc|rustdoc|cargo|cargo-clippy|clippy-driver [ARGS...]");
            return Ok(());
        }
        if !is_tool(tool) && !matches!(tool, "fetch" | "path") {
            return Err("unknown Rust tool".into());
        }
        if matches!(tool, "fetch" | "path") && !rest.is_empty() {
            return Err("fetch and path take no arguments".into());
        }
        let tree = ensure()?;
        if matches!(tool, "fetch" | "path") {
            println!("{}", tree.display());
            return Ok(());
        }
        let mut command = Command::new(tree.join("bin").join(tool));
        command.args(rest);
        for (key, bin) in [("RUSTC", "rustc"), ("RUSTDOC", "rustdoc")] {
            if std::env::var_os(key).is_none() {
                command.env(key, tree.join("bin").join(bin));
            }
        }
        Err(format!("run Rust {tool}: {}", command.exec()))
    })();
    if let Err(error) = result {
        eprintln!("td-rust: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use crate::rust_elf_synth as elf_synth;
    use crate::test_temp::TempDir;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn archive(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (name, contents) in files {
            let mut head = [0u8; 512];
            head[..name.len()].copy_from_slice(name.as_bytes());
            head[100..108].copy_from_slice(b"0000755\0");
            head[124..136].copy_from_slice(format!("{:011o}\0", contents.len()).as_bytes());
            head[148..156].fill(b' ');
            head[156] = b'0';
            head[257..263].copy_from_slice(b"ustar\0");
            let sum: u32 = head.iter().map(|&b| b as u32).sum();
            head[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            bytes.extend(head);
            bytes.extend(contents);
            bytes.resize(bytes.len().div_ceil(512) * 512, 0);
        }
        bytes.resize(bytes.len() + 1024, 0);
        bytes
    }

    fn fixture(root: &Path) -> (Config, BTreeMap<String, Vec<u8>>) {
        let executable = elf_synth::synth_interp_elf("/lib64/ld-linux-x86-64.so.2", true);
        let library = elf_synth::synth_needed_elf(&[], true);
        let mut rustc = Vec::new();
        for name in ["bin/rustc", "bin/rustdoc", LLD] {
            rustc.push((format!("r/rustc/{name}"), executable.clone()));
        }
        for name in [
            "manifest.in",
            "bin/rust-gdb",
            "bin/rust-gdbgui",
            "bin/rust-lldb",
            "libexec/stub",
            "lib/rustlib/x86_64-unknown-linux-gnu/bin/gcc-ld/ld.lld",
            "lib/rustlib/x86_64-unknown-linux-gnu/bin/rust-objcopy",
            "lib/rustlib/x86_64-unknown-linux-gnu/bin/wasm-component-ld",
        ] {
            rustc.push((format!("r/rustc/{name}"), b"unused".to_vec()));
        }
        rustc.push((
            "r/rustc/lib/librustc_driver-test.so".into(),
            library.clone(),
        ));
        rustc.push((
            "r/rustc/lib/libLLVM.so.23.1-rust-1.99.1-stable".into(),
            library,
        ));
        let rustc_refs: Vec<_> = rustc
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone()))
            .collect();
        let std_files: Vec<_> = ["std", "proc_macro", "test"].iter().map(|name| (format!("r/rust-std-x86_64-unknown-linux-gnu/lib/rustlib/x86_64-unknown-linux-gnu/lib/lib{name}-test.rlib"), vec![1])).collect();
        let std_refs: Vec<_> = std_files
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone()))
            .collect();
        let archives = BTreeMap::from([
            ("rustc".to_string(), archive(&rustc_refs)),
            ("std".to_string(), archive(&std_refs)),
            (
                "cargo".to_string(),
                archive(&[("r/cargo/bin/cargo", executable.clone())]),
            ),
            (
                "clippy".to_string(),
                archive(&[
                    ("r/clippy-preview/bin/cargo-clippy", executable.clone()),
                    ("r/clippy-preview/bin/clippy-driver", executable),
                ]),
            ),
        ]);
        let glibc = root.join("glibc");
        fs::create_dir_all(glibc.join("lib")).unwrap();
        for name in [
            "libc.so.6",
            "libdl.so.2",
            "librt.so.1",
            "libpthread.so.0",
            "libm.so.6",
            "libgcc_s.so.1",
            "libz.so.1",
            "ld-linux-x86-64.so.2",
        ] {
            fs::write(glibc.join("lib").join(name), b"library").unwrap();
        }
        let config = Config {
            identity: "a".repeat(64),
            version: "1.99.1".into(),
            libgcc: glibc.join("lib/libgcc_s.so.1"),
            libz: glibc.join("lib/libz.so.1"),
            glibc,
            sources: COMPONENTS
                .iter()
                .map(|(name, _)| Source {
                    name: (*name).into(),
                    url: (*name).into(),
                    digest: sha256::hex_digest(&archives[*name]),
                })
                .collect(),
        };
        (config, archives)
    }

    #[test]
    fn configuration_binds_version_pins_and_runtime_paths() {
        let mut text = String::from("td-rust-v1\n1.99.1\n/td/store/glibc/libc\n/td/store/gcc/libgcc_s.so.1\n/td/store/zlib/libz.so.1\n");
        for (name, _) in COMPONENTS {
            text.push_str(&format!(
                "{name}\thttps://static.rust-lang.org/dist/{name}.tar.xz\t{}\n",
                "a".repeat(64)
            ));
        }
        let parsed = Config::parse(text.as_bytes()).unwrap();
        assert_eq!(parsed.version, "1.99.1");
        assert_eq!(parsed.identity, sha256::hex_digest(text.as_bytes()));
        for broken in [
            text.replace("td-rust-v1", "td-rust-v2"),
            text.replace("https://", "http://"),
            text.replace("static.rust-lang.org/", "static.rust-lang.org.evil/"),
            text.replace("/td/store/glibc", "/usr/glibc"),
            text.replace("/td/store/glibc", "/td/store/../glibc"),
            text.replace(&"a".repeat(64), &"a".repeat(63)),
            text.replace("cargo\t", "rustc\t"),
            format!("{text}extra\n"),
        ] {
            assert!(Config::parse(broken.as_bytes()).is_err(), "{broken}");
        }
        let changed = text.replace("1.99.1", "1.100.0");
        assert_ne!(
            parsed.identity,
            Config::parse(changed.as_bytes()).unwrap().identity
        );
    }

    #[test]
    fn cold_fetch_retargets_and_publishes_once_then_works_offline() {
        let d = TempDir::new("rust-cold").unwrap();
        let (config, archives) = fixture(&d);
        let root = d.join("cache");
        let uid = fs::metadata(&*d).unwrap().uid();
        let calls = AtomicUsize::new(0);
        let acquire = |name: &str, path: &Path| {
            calls.fetch_add(1, Ordering::SeqCst);
            fs::write(path, &archives[name]).map_err(|e| e.to_string())
        };
        let results = std::thread::scope(|scope| {
            let a = scope.spawn(|| prepare(&config, &root, uid, acquire));
            let b = scope.spawn(|| prepare(&config, &root, uid, acquire));
            (a.join().unwrap(), b.join().unwrap())
        });
        let tree = results.0.unwrap();
        assert_eq!(tree, results.1.unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            td_engine::elf::read_interp(&tree.join("bin/rustc")).unwrap(),
            Some(format!(
                "{}/lib/ld-linux-x86-64.so.2",
                config.glibc.display()
            ))
        );
        assert!(!root.join(".work").exists());
        assert_eq!(
            prepare(&config, &root, uid, |_, _| Err("offline".into())).unwrap(),
            tree
        );
        fs::write(tree.join("bin/rustc"), b"bad").unwrap();
        assert!(prepare(&config, &root, uid, |_, _| panic!(
            "cached damage must not execute or fetch"
        ))
        .is_err());
    }

    #[test]
    fn rejected_or_interrupted_download_never_publishes() {
        let d = TempDir::new("rust-fail").unwrap();
        let (config, archives) = fixture(&d);
        let root = d.join("cache");
        let uid = fs::metadata(&*d).unwrap().uid();
        let error = prepare(&config, &root, uid, |_, path| {
            fs::write(path, b"wrong").map_err(|e| e.to_string())
        })
        .unwrap_err();
        assert!(error.contains("checksum"));
        assert!(!root.join(&config.identity).exists());
        assert!(!root.join(".work").exists());
        assert!(prepare(&config, &root, uid, |_, path| {
            fs::write(path, b"partial").unwrap();
            Err("interrupted".into())
        })
        .is_err());
        fs::create_dir(root.join(".work")).unwrap();
        fs::write(root.join(".work/abandoned"), b"old").unwrap();
        assert!(prepare(&config, &root, uid, |name, path| fs::write(
            path,
            &archives[name]
        )
        .map_err(|e| e.to_string()))
        .is_ok());
        let elsewhere = d.join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();
        let linked = d.join("linked");
        std::os::unix::fs::symlink(&elsewhere, &linked).unwrap();
        assert!(prepare(&config, &linked, uid, |_, _| panic!("unsafe cache")).is_err());
        assert_eq!(fs::read_dir(elsewhere).unwrap().count(), 0);
    }
}
