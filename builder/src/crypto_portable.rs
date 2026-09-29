//! Host-only, pinned inputs for the standalone musl artifact.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, String>;

const HOST: &str = "x86_64-unknown-linux-gnu";
const TARGET: &str = "x86_64-unknown-linux-musl";
const DIST: &str = "https://static.rust-lang.org/dist/2026-05-28";

struct Archive {
    name: &'static str,
    component: &'static str,
    bytes: u64,
    sha256: &'static str,
}

const RUST: [Archive; 4] = [
    Archive {
        name: "rustc-1.96.0-x86_64-unknown-linux-gnu",
        component: "rustc",
        bytes: 80_188_348,
        sha256: "7d7fa1d0cfb0fab71a956bb78f41107202c17f30ab56c45288e869a37fd9633d",
    },
    Archive {
        name: "cargo-1.96.0-x86_64-unknown-linux-gnu",
        component: "cargo",
        bytes: 11_154_668,
        sha256: "dee75c3c8f9f600ad75bc0c93249e767d3047845a4dd668327ce43ab039ba266",
    },
    Archive {
        name: "rust-std-1.96.0-x86_64-unknown-linux-gnu",
        component: "rust-std-x86_64-unknown-linux-gnu",
        bytes: 29_740_704,
        sha256: "c09c7c646248f14f473f5f7a029af15ee57c3a9f9bc93dfa72d9621938586b82",
    },
    Archive {
        name: "rust-std-1.96.0-x86_64-unknown-linux-musl",
        component: "rust-std-x86_64-unknown-linux-musl",
        bytes: 38_833_276,
        sha256: "4db24564076c243b377585bec0b938388638301ea5003d6245c4cbf9d86b11d0",
    },
];

struct NativeInput {
    graph: &'static str,
    recipe: &'static str,
    directory: &'static str,
    inside: &'static str,
}

const NATIVE: [NativeInput; 5] = [
    NativeInput {
        graph: "gcc-x86-64-self",
        recipe: "gcc-x86-64-self",
        directory: "stage/td/store/gcc-14.3.0-x86_64-self",
        inside: "/cc",
    },
    NativeInput {
        graph: "gcc-x86-64-self",
        recipe: "binutils-x86-64-self",
        directory: "",
        inside: "/binutils",
    },
    NativeInput {
        graph: "gcc-x86-64-self",
        recipe: "glibc-x86-64",
        directory: "stage/td/store/glibc-2.41-x86_64/lib",
        inside: "/lib64",
    },
    NativeInput {
        graph: "gcc-x86-64-self",
        recipe: "gcc-x86-64-stage2",
        directory: "stage/td/store/gcc-14.3.0-x86_64/x86_64-pc-linux-gnu/lib64",
        inside: "/gcc-runtime",
    },
    NativeInput {
        graph: "zlib-x86-64",
        recipe: "zlib-x86-64",
        directory: "stage/td/store/zlib-1.3.1/lib",
        inside: "/zlib",
    },
];

struct NativeTree {
    inside: &'static str,
    directory: PathBuf,
    record: String,
}

#[derive(Default)]
struct NativeLog {
    outputs: BTreeMap<String, PathBuf>,
    work: Option<PathBuf>,
    tail: String,
    invalid: Option<String>,
}

fn absolute_record(text: &str) -> Result<PathBuf> {
    let path = PathBuf::from(text);
    if !path.is_absolute()
        || path
            .components()
            .any(|p| matches!(p, std::path::Component::ParentDir))
    {
        return Err("native output path must be absolute without parent traversal".into());
    }
    Ok(path)
}

fn native_record(
    line: &[u8],
    truncated: bool,
    inputs: &[&NativeInput],
    log: &mut NativeLog,
) -> Result<()> {
    if !line.starts_with(b"TD_RECIPE_RUN_OUT ") && !line.starts_with(b"TD_RECIPE_RUN_WORK ") {
        return Ok(());
    }
    if truncated || !line.ends_with(b"\n") {
        return Err("overlong or unterminated native output record".into());
    }
    let text = std::str::from_utf8(line)
        .map_err(|e| format!("native record UTF-8: {e}"))?
        .trim_end_matches(['\r', '\n']);
    if let Some(path) = text.strip_prefix("TD_RECIPE_RUN_WORK ") {
        if log.work.replace(absolute_record(path)?).is_some() {
            return Err("duplicate native work record".into());
        }
    } else if let Some(record) = text.strip_prefix("TD_RECIPE_RUN_OUT ") {
        let (name, path) = record
            .split_once(' ')
            .ok_or("malformed native output record")?;
        if !inputs.iter().any(|input| input.recipe == name) {
            return Err(format!("unexpected native output record: {name}"));
        }
        if log
            .outputs
            .insert(name.into(), absolute_record(path)?)
            .is_some()
        {
            return Err(format!("duplicate native output record: {name}"));
        }
    }
    Ok(())
}

fn native_log(mut reader: impl BufRead, inputs: &[&NativeInput]) -> Result<NativeLog> {
    let mut log = NativeLog::default();
    let mut tail = VecDeque::new();
    loop {
        let mut line = Vec::new();
        let mut truncated = false;
        let mut ended = false;
        loop {
            let bytes = reader
                .fill_buf()
                .map_err(|e| format!("read native log: {e}"))?;
            if bytes.is_empty() {
                break;
            }
            let count = if let Some(end) = bytes.iter().position(|b| *b == b'\n') {
                ended = true;
                end + 1
            } else {
                bytes.len()
            };
            let keep = count.min(65_536usize.saturating_sub(line.len()));
            line.extend_from_slice(bytes.get(..keep).ok_or("native log buffer bounds")?);
            truncated |= keep < count;
            reader.consume(count);
            if ended {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        if let Err(e) = native_record(&line, truncated, inputs, &mut log) {
            if log.invalid.is_none() {
                log.invalid = Some(e);
            }
        }
        let mut diagnostic = String::from_utf8_lossy(&line).into_owned();
        if truncated {
            diagnostic.push_str("… [line truncated]\n");
        }
        if tail.len() == 30 {
            tail.pop_front();
        }
        tail.push_back(diagnostic);
        if !ended {
            break;
        }
    }
    log.tail = tail.into_iter().collect();
    Ok(log)
}

fn durable_output(work: &Path, staged: &Path) -> Result<PathBuf> {
    if !staged.starts_with(work.join("scratch")) {
        return Err("native staged output is outside evaluator scratch".into());
    }
    let name = staged.file_name().ok_or("native output lacks a name")?;
    // The evaluator publishes complete cache trees and reclaims them by
    // rename, whereas scratch cleanup may leave a partially deleted tree.
    Ok(work.join("build-cache/store").join(name))
}

fn role_directory(output: &Path, role: &str) -> Result<PathBuf> {
    if !fs::symlink_metadata(output).is_ok_and(|m| m.is_dir()) {
        return Err("native cache root is not a real directory".into());
    }
    let directory = if role.is_empty() {
        output.to_path_buf()
    } else {
        output.join(role)
    };
    if !fs::symlink_metadata(&directory).is_ok_and(|m| m.is_dir()) {
        return Err(format!(
            "native recipe layout changed: {}",
            directory.display()
        ));
    }
    let directory = directory
        .canonicalize()
        .map_err(|e| format!("resolve native input: {e}"))?;
    let canonical_output = output
        .canonicalize()
        .map_err(|e| format!("resolve native output: {e}"))?;
    if !directory.starts_with(canonical_output) {
        return Err("native input directory escapes its recipe output".into());
    }
    Ok(directory)
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

fn publish_tree(source: &Path, parent: &Path, label: &str, expected: &str) -> Result<PathBuf> {
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

fn retain_native(
    source: &Path,
    parent: &Path,
    scratch: &Scratch,
    name: &str,
) -> Result<(PathBuf, String)> {
    let expected = tree_hash(source)?;
    let destination = cache_path(parent, name, &expected)?;
    if verified_cache(&destination, &expected)? {
        return Ok((destination, expected));
    }
    let private = scratch.0.join(name);
    crate::build::copy_tree_writable(source, &private)?;
    if tree_hash(&private)? != expected || tree_hash(source)? != expected {
        return Err("native output changed during retention; retry preparation".into());
    }
    let destination = publish_tree(&private, parent, name, &expected)?;
    Ok((destination, expected))
}

fn native_outputs(root: &Path) -> Result<Vec<NativeTree>> {
    let parent = root.join(".td-build-cache");
    let evaluator = crate::stage0::recipe_eval_place(root, &parent.join("recipe-eval"))?;
    let scratch = Scratch::new(&parent)?;
    let log_path = scratch.0.join("native.log");
    eprintln!(
        "portable: realizing the declared td GNU inputs (a cold source bootstrap can take hours)"
    );
    let mut trees = Vec::new();
    for graph in NATIVE
        .iter()
        .map(|input| input.graph)
        .collect::<BTreeSet<_>>()
    {
        let inputs: Vec<_> = NATIVE.iter().filter(|input| input.graph == graph).collect();
        let log =
            fs::File::create(&log_path).map_err(|e| format!("create native build log: {e}"))?;
        let mut command = Command::new(&evaluator);
        command.current_dir(root).arg("build-run").arg(graph);
        for input in &inputs {
            command.arg(input.recipe);
        }
        command
            .stdout(
                log.try_clone()
                    .map_err(|e| format!("clone native log: {e}"))?,
            )
            .stderr(log);
        crate::host_bin::arm_check_child(&mut command);
        let status = command
            .status()
            .map_err(|e| format!("realize portable native inputs: {e}"))?;
        let reader = BufReader::new(fs::File::open(&log_path).map_err(|e| {
            format!("portable native graph {graph} exited ({status}); log open failed: {e}")
        })?);
        let log = native_log(reader, &inputs).map_err(|e| {
            format!("portable native graph {graph} exited ({status}); log read failed: {e}")
        })?;
        if !status.success() {
            return Err(format!(
                "portable native graph {graph} failed ({status}):\n{}",
                log.tail
            ));
        }
        if let Some(error) = log.invalid {
            return Err(format!(
                "portable native graph {graph} exited ({status}); {error}"
            ));
        }
        let work = log
            .work
            .ok_or("native realization did not return its work directory")?;
        for input in inputs {
            let staged = log
                .outputs
                .get(input.recipe)
                .ok_or_else(|| format!("native realization did not return {}", input.recipe))?;
            let identity = staged
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or("native output lacks a UTF-8 name")?;
            let source = durable_output(&work, staged)?;
            let (output, digest) = retain_native(&source, &parent, &scratch, input.recipe)?;
            let directory = role_directory(&output, input.directory)?;
            trees.push(NativeTree {
                inside: input.inside,
                directory,
                record: format!("native {} {identity} {digest}\n", input.recipe),
            });
        }
    }
    Ok(trees)
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

fn rust_receipt() -> String {
    let mut text = format!("portable-inputs 1\nhost {HOST}\ntarget {TARGET}\n");
    for pin in &RUST {
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

fn prepare_rust(root: &Path, archives: &Path) -> Result<PathBuf> {
    let parent = root.join(".td-build-cache");
    let scratch = Scratch::new(&parent)?;
    let kit = scratch.0.join("kit");
    for pin in &RUST {
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
    fs::write(kit.join("PORTABLE-INPUTS"), rust_receipt())
        .map_err(|e| format!("write portable Rust receipt: {e}"))?;
    let expected = tree_hash(&kit)?;
    publish_tree(&kit, &parent, "rust-1.96.0", &expected)
}

pub(crate) fn prepare(root: &Path, archives: &Path) -> Result<()> {
    let kit = prepare_rust(root, archives)?;
    let headers = crate::crypto_headers::prepare(root, &archives.join("musl-1.2.5.tar.gz"))?;
    let native = native_outputs(root)?;
    println!("portable-rust {}", kit.display());
    println!("portable-musl-headers {}", headers.display());
    for tree in native {
        print!("{}", tree.record);
        println!(
            "portable-native-bind {} {}",
            tree.inside,
            tree.directory.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::os::unix::fs::{symlink, PermissionsExt};

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
    fn native_records_reject_ambiguity_and_bound_logs() {
        let inputs = vec![NATIVE.first().unwrap()];
        let good = "building\nTD_RECIPE_RUN_OUT gcc-x86-64-self /a/path with spaces\n";
        let log = native_log(Cursor::new(good), &inputs).unwrap();
        assert!(log.invalid.is_none());
        assert_eq!(
            log.outputs.get("gcc-x86-64-self").unwrap(),
            Path::new("/a/path with spaces")
        );
        for bad in [
            format!("{good}{good}"),
            "TD_RECIPE_RUN_OUT other /path\n".into(),
            "TD_RECIPE_RUN_OUT gcc-x86-64-self relative\n".into(),
            "TD_RECIPE_RUN_OUT gcc-x86-64-self /a/../b\n".into(),
            "TD_RECIPE_RUN_OUT gcc-x86-64-self /path".into(),
            format!(
                "TD_RECIPE_RUN_OUT gcc-x86-64-self /{}\n",
                "x".repeat(65_537)
            ),
        ] {
            assert!(native_log(Cursor::new(bad), &inputs)
                .unwrap()
                .invalid
                .is_some());
        }
        let log = native_log(Cursor::new("line\n".repeat(100)), &inputs).unwrap();
        assert_eq!(log.tail.lines().count(), 30);
        let mut noisy = vec![0xff; 90_000];
        noisy.extend_from_slice(b"\n");
        noisy.extend_from_slice(good.as_bytes());
        let log = native_log(Cursor::new(noisy), &inputs).unwrap();
        assert!(log.invalid.is_none());
        assert_eq!(log.outputs.len(), 1);
        assert!(log.tail.contains("truncated"));
    }

    #[test]
    fn retained_native_bytes_survive_evaluator_reaping() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let source = scratch.0.join("evaluator-output");
        let parent = scratch.0.join("cache");
        fs::create_dir_all(source.join("bin")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::write(source.join("bin/tool"), b"tool").unwrap();
        fs::set_permissions(source.join("bin/tool"), fs::Permissions::from_mode(0o755)).unwrap();
        symlink("tool", source.join("bin/alias")).unwrap();
        let expected = tree_hash(&source).unwrap();
        let (retained, digest) = retain_native(&source, &parent, &scratch, "fixture").unwrap();
        // A warm reuse must not attempt a new private copy.
        fs::write(scratch.0.join("fixture"), b"would block copying").unwrap();
        assert_eq!(
            retain_native(&source, &parent, &scratch, "fixture")
                .unwrap()
                .0,
            retained
        );
        fs::remove_dir_all(&source).unwrap();
        assert_eq!(digest, expected);
        assert_eq!(tree_hash(&retained).unwrap(), expected);
        assert_eq!(fs::read(retained.join("bin/alias")).unwrap(), b"tool");
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
    fn roles_cannot_escape_or_use_symlink_roots() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let output = scratch.0.join("output");
        fs::create_dir_all(output.join("stage/lib")).unwrap();
        assert_eq!(
            role_directory(&output, "stage/lib").unwrap(),
            output.join("stage/lib")
        );
        assert_eq!(role_directory(&output, "").unwrap(), output);
        assert!(role_directory(&output, "missing").is_err());
        let alias = scratch.0.join("alias");
        symlink(&output, &alias).unwrap();
        assert!(role_directory(&alias, "stage/lib").is_err());
        let outside = scratch.0.join("outside");
        fs::create_dir_all(outside.join("lib")).unwrap();
        fs::remove_dir_all(output.join("stage")).unwrap();
        symlink(&outside, output.join("stage")).unwrap();
        assert!(role_directory(&output, "stage/lib")
            .unwrap_err()
            .contains("escapes"));
        assert!(role_directory(&output, "stage").is_err());
    }

    #[test]
    fn evaluator_staging_is_not_the_native_copy_authority() {
        let scratch = Scratch::new(&std::env::temp_dir()).unwrap();
        let work = scratch.0.join("work");
        let staged = work.join("scratch/run/tdstore/hash-recipe");
        let source = work.join("build-cache/store/hash-recipe");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("tool"), b"complete recipe output").unwrap();
        // An interrupted scratch reaper may leave only an empty directory.
        assert_ne!(tree_hash(&staged).unwrap(), tree_hash(&source).unwrap());
        assert_eq!(durable_output(&work, &staged).unwrap(), source);
        fs::remove_dir_all(&staged).unwrap();
        assert_eq!(durable_output(&work, &staged).unwrap(), source);
        assert!(durable_output(&work, Path::new("/elsewhere/output")).is_err());
        let inputs = vec![NATIVE.first().unwrap()];
        let valid = "TD_RECIPE_RUN_WORK /work\n";
        assert_eq!(
            native_log(Cursor::new(valid), &inputs)
                .unwrap()
                .work
                .unwrap(),
            Path::new("/work")
        );
        assert!(native_log(Cursor::new(format!("{valid}{valid}")), &inputs)
            .unwrap()
            .invalid
            .is_some());
    }
}
