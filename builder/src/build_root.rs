//! build_root — the Debian build root (BUILD-ROOT-PLAN.md, increment 3).
//!
//! A C build environment, never a source of shipped bytes: the lock's pinned
//! Debian image layer with its .debs unpacked over it in lock order,
//! assembled into one input-addressed store path and mounted read-only as
//! `/` for a recipe that declares `buildRoot` (sandbox.rs). The lock pins
//! every input by sha256 and the assembled tree by NAR hash, so a root is
//! admitted only when it is exactly the pinned assembly.
//!
//! No maintainer script runs. What dpkg's scripts and the slim image's
//! dpkg configuration would do is reproduced by three tables: the usrmerge
//! links a .deb path is written through, the paths the slim image excludes,
//! and the update-alternatives links the build tools need.

use crate::{sandbox, scan, sha256, store, store_db, store_db_read, tar, xz};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use td_engine::debian_root::{self, Kind, Lock, Pin};

const LOCK: &str = include_str!("../../recipes/src/debian-root.lock");

/// The store name of the assembled tree.
pub(crate) const NAME: &str = "debian-build-root";

/// Folded into the store key: bump when the assembly below changes what a
/// lock produces.
const ASSEMBLY: &str = "td-debian-build-root-v1";

/// The image's top-level usrmerge links: a .deb entry under one lands under
/// its target, as dpkg would write it through the link.
const USRMERGE: &[(&str, &str)] = &[
    ("bin", "usr/bin"),
    ("lib", "usr/lib"),
    ("lib64", "usr/lib64"),
    ("sbin", "usr/sbin"),
];

/// The slim image's dpkg path-excludes; .deb payload under them is dropped.
const EXCLUDES: &[&str] = &[
    "usr/share/doc",
    "usr/share/groff",
    "usr/share/info",
    "usr/share/linda",
    "usr/share/lintian",
    "usr/share/locale",
    "usr/share/man",
];

/// The update-alternatives links the build tools need, as (link, target
/// relative to the link's directory). Created only where the link is absent;
/// each target must exist, so a lock that drops one fails assembly.
const ALTERNATIVES: &[(&str, &str)] = &[
    ("usr/bin/c89", "c89-gcc"),
    ("usr/bin/c99", "c99-gcc"),
    ("usr/bin/cc", "gcc"),
    ("usr/bin/lzma", "xz"),
    ("usr/bin/unlzma", "unxz"),
    ("usr/bin/yacc", "bison.yacc"),
    ("usr/lib/cpp", "../bin/cpp"),
];

/// Mount points the sandbox needs in a root it cannot write.
const SKELETON: &[&str] = &["dev", "etc", "oldroot", "proc", "td/store", "tmp"];

/// The lock td-builder was compiled with.
pub(crate) fn lock() -> Result<Lock, String> {
    debian_root::parse(LOCK)
}

/// The input-addressed key: the assembly revision, every pin in order, and
/// the pinned tree. The tree is in it so two locks that share pins but pin
/// different trees name different paths (and dbs), rather than one path
/// whose bytes each would replace in turn.
pub(crate) fn key(lock: &Lock) -> Result<String, String> {
    let mut text = format!("{ASSEMBLY}\n");
    for pin in &lock.pins {
        text.push_str(&format!("{} {}\n", pin.file, pin.sha256));
    }
    text.push_str(&format!("tree {}\n", pinned_tree(lock)?));
    Ok(sha256::hex_digest(text.as_bytes()))
}

/// The store path a recipe declaring `buildRoot` stages as its root.
pub(crate) fn store_path(lock: &Lock) -> Result<String, String> {
    Ok(store::input_addressed_path(&key(lock)?, NAME))
}

/// Where the root's registration lives: beside the seed db, named by key so
/// worktrees on different locks share one seed store.
pub(crate) fn db_path(seed_db: &Path, lock: &Lock) -> Result<PathBuf, String> {
    let key = key(lock)?;
    Ok(seed_db.with_file_name(format!("{NAME}-{}.db", key.get(..16).unwrap_or(&key))))
}

/// The compiled lock's store path and pinned tree hash: the only item the
/// sandbox binds as `/`.
pub(crate) fn pinned() -> Result<(String, String), String> {
    let lock = lock()?;
    Ok((store_path(&lock)?, pinned_tree(&lock)?.to_string()))
}

fn pinned_tree(lock: &Lock) -> Result<&str, String> {
    lock.tree.as_deref().ok_or_else(|| {
        "recipes/src/debian-root.lock has no `tree' row: run `td-builder build-root pin'".into()
    })
}

fn read_pin(sources: &Path, pin: &Pin) -> Result<Vec<u8>, String> {
    let path = sources.join(&pin.file);
    let bytes = fs::read(&path)
        .map_err(|e| format!("{}: {e} (run `td-feed warm build-root')", path.display()))?;
    if u64::try_from(bytes.len()).ok() != Some(pin.size) || sha256::hex_digest(&bytes) != pin.sha256
    {
        return Err(format!(
            "{} does not match its pin {}",
            path.display(),
            pin.key
        ));
    }
    Ok(bytes)
}

/// The `data.tar.xz` member of a .deb (an `ar` archive). Other payload
/// compressions are refused, as is any archive this reader cannot frame.
fn deb_data<'a>(deb: &'a [u8], name: &str) -> Result<&'a [u8], String> {
    let bad = |why: &str| format!("{name}: {why}");
    let mut rest = deb
        .strip_prefix(b"!<arch>\n")
        .ok_or_else(|| bad("not an ar archive"))?;
    let mut version_seen = false;
    while !rest.is_empty() {
        let header = rest.get(..60).ok_or_else(|| bad("truncated ar header"))?;
        if header.get(58..60) != Some(b"`\n".as_slice()) {
            return Err(bad("malformed ar header"));
        }
        let field = |range: std::ops::Range<usize>| {
            header
                .get(range)
                .and_then(|f| std::str::from_utf8(f).ok())
                .map(str::trim_end)
                .unwrap_or_default()
        };
        let member = field(0..16).trim_end_matches('/');
        let size: usize = field(48..58)
            .parse()
            .map_err(|_| bad("malformed ar size"))?;
        let body = rest
            .get(
                60..60usize
                    .checked_add(size)
                    .ok_or_else(|| bad("ar size overflows"))?,
            )
            .ok_or_else(|| bad("truncated ar member"))?;
        match member {
            "debian-binary" if body == b"2.0\n" => version_seen = true,
            "debian-binary" => return Err(bad("not a 2.0 .deb")),
            "data.tar.xz" if version_seen => return Ok(body),
            "data.tar.xz" => return Err(bad("data.tar.xz precedes debian-binary")),
            other if other.starts_with("data.tar") => {
                return Err(bad(&format!(
                    "payload `{other}': td reads data.tar.xz only"
                )));
            }
            _ => {}
        }
        let next = 60usize
            .checked_add(size)
            .and_then(|end| end.checked_add(size % 2))
            .ok_or_else(|| bad("ar size overflows"))?;
        rest = rest.get(next..).unwrap_or_default();
    }
    Err(bad("no data.tar.xz member"))
}

fn excluded(rel: &str) -> bool {
    EXCLUDES.iter().any(|ex| {
        rel == *ex
            || rel
                .strip_prefix(ex)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

/// Where a top-level .deb path lands: through a usrmerge link when the root
/// has that exact link, else where it is named.
fn usrmerged(root: &Path, name: &str) -> String {
    let Some((_, target)) = USRMERGE.iter().find(|(link, _)| *link == name) else {
        return name.to_string();
    };
    match fs::read_link(root.join(name)) {
        Ok(found) if found == Path::new(target) => target.to_string(),
        _ => name.to_string(),
    }
}

/// Move `src`'s tree into `root`, as unpacking a .deb over it would: a
/// directory merges into a directory, anything else replaces a non-directory.
/// Nothing is written through a symlink other than the usrmerge links.
fn merge(src: &Path, root: &Path, rel: &str) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .collect::<Result<_, _>>()
        .map_err(|e| format!("read {}: {e}", src.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("{}: non-UTF-8 name", entry.path().display()))?;
        let target = if rel.is_empty() {
            usrmerged(root, name)
        } else {
            format!("{rel}/{name}")
        };
        if excluded(&target) {
            continue;
        }
        let dest = root.join(&target);
        let kind = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
        let existing = match fs::symlink_metadata(&dest) {
            Ok(meta) => Some(meta.file_type()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("stat {}: {e}", dest.display())),
        };
        if kind.is_dir() {
            match existing {
                Some(found) if found.is_dir() => {}
                Some(_) => {
                    return Err(format!("{target}: a directory over a non-directory"));
                }
                None => fs::create_dir(&dest).map_err(|e| format!("mkdir {target}: {e}"))?,
            }
            merge(&entry.path(), root, &target)?;
        } else {
            match existing {
                Some(found) if found.is_dir() => {
                    return Err(format!("{target}: a non-directory over a directory"));
                }
                Some(_) => fs::remove_file(&dest).map_err(|e| format!("replace {target}: {e}"))?,
                None => {}
            }
            fs::rename(entry.path(), &dest).map_err(|e| format!("place {target}: {e}"))?;
        }
    }
    Ok(())
}

fn refuse_whiteouts(dir: &Path) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        if entry.file_name().to_string_lossy().starts_with(".wh.") {
            return Err(format!(
                "{}: the image layer carries a whiteout; td takes a single-layer image",
                entry.path().display()
            ));
        }
        let kind = entry
            .file_type()
            .map_err(|e| format!("stat {}: {e}", entry.path().display()))?;
        if kind.is_dir() {
            refuse_whiteouts(&entry.path())?;
        }
    }
    Ok(())
}

fn add_alternatives(root: &Path) -> Result<(), String> {
    for (link, target) in ALTERNATIVES {
        let path = root.join(link);
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        let dir = path.parent().ok_or_else(|| format!("{link}: no parent"))?;
        // Not `metadata`: following an absolute link would test the host.
        if fs::symlink_metadata(dir.join(target)).is_err() {
            return Err(format!(
                "alternative {link} -> {target}: the target is absent"
            ));
        }
        std::os::unix::fs::symlink(target, &path).map_err(|e| format!("link {link}: {e}"))?;
    }
    Ok(())
}

fn add_skeleton(root: &Path) -> Result<(), String> {
    for dir in SKELETON {
        let path = root.join(dir);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("{dir}: a mount point that is not a directory")),
            Err(_) => fs::create_dir_all(&path).map_err(|e| format!("mkdir {dir}: {e}"))?,
        }
    }
    Ok(())
}

/// Assemble the tree under `work` (created empty) from the pins in `sources`.
pub(crate) fn assemble(lock: &Lock, sources: &Path, work: &Path) -> Result<PathBuf, String> {
    let root = work.join("root");
    let unpack = work.join("unpack");
    fs::create_dir_all(&root).map_err(|e| format!("mkdir {}: {e}", root.display()))?;
    for pin in &lock.pins {
        let bytes = read_pin(sources, pin)?;
        match pin.kind {
            Kind::Layer => {
                let tar_bytes = td_engine::gzip::decompress_bytes(&bytes)
                    .map_err(|e| format!("{}: {e}", pin.file))?;
                tar::extract_tar_reader(&mut tar_bytes.as_slice(), &pin.file, &root)?;
                refuse_whiteouts(&root)?;
            }
            Kind::Deb => {
                let data = deb_data(&bytes, &pin.file)?;
                let mut tar_bytes = Vec::new();
                xz::decompress_reader_to(&mut &data[..], &mut tar_bytes)
                    .map_err(|e| format!("{}: {e}", pin.file))?;
                tar::extract_tar_reader(&mut tar_bytes.as_slice(), &pin.file, &unpack)?;
                merge(&unpack, &root, "")?;
                fs::remove_dir_all(&unpack)
                    .map_err(|e| format!("clear {}: {e}", unpack.display()))?;
            }
        }
    }
    add_alternatives(&root)?;
    add_skeleton(&root)?;
    Ok(root)
}

/// The root's registration must be exactly the compiled lock's store path at
/// its pinned tree hash, and its bytes under `items_dir` must hash to it.
/// Checked once per process per (db, dir); the sandbox re-hashes the bytes
/// it binds regardless.
pub(crate) fn authenticate_db(db: &Path, items_dir: &Path) -> Result<String, String> {
    static DONE: OnceLock<Mutex<Vec<(PathBuf, PathBuf)>>> = OnceLock::new();
    let lock = lock()?;
    let path = store_path(&lock)?;
    let memo = (db.to_path_buf(), items_dir.to_path_buf());
    let done = DONE.get_or_init(|| Mutex::new(Vec::new()));
    if done.lock().is_ok_and(|seen| seen.contains(&memo)) {
        return Ok(path);
    }
    authenticate_db_with(&path, pinned_tree(&lock)?, db, items_dir)?;
    if let Ok(mut seen) = done.lock() {
        seen.push(memo);
    }
    Ok(path)
}

fn authenticate_db_with(path: &str, tree: &str, db: &Path, items_dir: &Path) -> Result<(), String> {
    let fail = |why: String| format!("build root db {}: provenance rejected: {why}", db.display());
    let data = fs::read(db).map_err(|e| {
        fail(format!(
            "read: {e} (td-recipe-eval runs `td-builder build-root assemble' before planning)"
        ))
    })?;
    let hashes = store_db_read::Db::open(data)
        .and_then(|db| db.hashes_by_path())
        .map_err(fail)?;
    if hashes.len() != 1 || hashes.get(path).map(String::as_str) != Some(tree) {
        return Err(fail(format!(
            "it must register exactly {path} at the pinned {tree}"
        )));
    }
    let base = path.rsplit('/').next().unwrap_or_default();
    let have = sandbox::nar_hash_of(&items_dir.join(base))
        .map_err(|e| fail(format!("hash {base}: {e}")))?;
    if have != tree {
        return Err(fail(format!(
            "{base} on disk hashes {have}, pinned as {tree}"
        )));
    }
    Ok(())
}

/// Register the placed tree as the single row of a fresh store db.
fn write_db(path: &str, disk: &Path, out_db: &Path) -> Result<(), String> {
    use store_db::{Table, Value};
    let closure = vec![path.to_string()];
    let mut scanner = scan::Scanner::new(&closure).map_err(|e| e.to_string())?;
    crate::nar::write_nar(&mut scanner, disk).map_err(|e| e.to_string())?;
    let (hash, size, refs) = scanner.finish();
    if !refs.is_empty() {
        return Err(format!("{path} names its own store path"));
    }
    let tables = [
        Table {
            name: "ValidPaths",
            sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
            rows: vec![(
                1i64,
                vec![
                    Value::Null,
                    Value::Text(path.to_string()),
                    Value::Text(hash),
                    Value::Int(1),
                    Value::Null,
                    Value::Int(i64::try_from(size).map_err(|_| "NAR size overflows")?),
                ],
            )],
        },
        Table {
            name: "Refs",
            sql: "CREATE TABLE Refs (referrer integer, reference integer)",
            rows: Vec::new(),
        },
    ];
    let tmp = out_db.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, store_db::write_db(&tables))
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, out_db).map_err(|e| format!("commit {}: {e}", out_db.display()))
}

/// A scratch dir, cleared before use and removed on drop.
struct Work(PathBuf);

impl Work {
    fn new(path: PathBuf) -> Result<Work, String> {
        if fs::symlink_metadata(&path).is_ok() {
            crate::remove_store_path(&path)?;
        }
        fs::create_dir_all(&path).map_err(|e| format!("mkdir {}: {e}", path.display()))?;
        Ok(Work(path))
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        let _ = crate::remove_store_path(&self.0);
    }
}

/// `build-root assemble SOURCES SEED-STORE SEED-DB`: make the pinned root
/// present in SEED-STORE with its db beside SEED-DB; a no-op when an
/// authenticated one is already there. Prints the store path.
fn ensure(sources: &Path, seed_store: &Path, seed_db: &Path) -> Result<String, String> {
    let lock = lock()?;
    let tree = pinned_tree(&lock)?.to_string();
    let path = store_path(&lock)?;
    let db = db_path(seed_db, &lock)?;
    // The seed store's commit lock, as every seed-store writer takes it: a
    // second worktree waits here, then finds the root the first one placed.
    let _commit_lock = crate::lock_store_commit(seed_db)?;
    if db.is_file() && authenticate_db_with(&path, &tree, &db, seed_store).is_ok() {
        return Ok(path);
    }
    let base = path.rsplit('/').next().unwrap_or_default();
    // A fixed name under the lock: a killed assembly's leftover is cleared by
    // the next one rather than kept beside the store forever.
    let work = Work::new(seed_store.join(format!(".{NAME}-work")))?;
    eprintln!(
        "td-builder: build-root: assembling {path} from {} pins",
        lock.pins.len()
    );
    let root = assemble(&lock, sources, &work.0)?;
    let got = sandbox::nar_hash_of(&root).map_err(|e| format!("hash the assembled root: {e}"))?;
    if got != tree {
        return Err(format!(
            "the assembled root hashes {got}, but the lock pins {tree}: the assembly is not \
             reproducible, or the pin is stale (`td-builder build-root pin')"
        ));
    }
    let disk = seed_store.join(base);
    // Registered: the path names exactly this tree, so a different tree
    // already there is damage to refuse, never a stale copy to replace.
    crate::commit_tree_checked(&root, &disk, &tree, true, false).map_err(|e| {
        format!(
            "{e}\n(for the build root, a tree at its path that hashes otherwise is damage: \
             remove {} and rerun)",
            disk.display()
        )
    })?;
    write_db(&path, &disk, &db)?;
    authenticate_db(&db, seed_store)
}

/// `build-root pin SOURCES LOCK-FILE WORK`: assemble the compiled lock's
/// pins under WORK and write the tree's NAR hash into LOCK-FILE's `tree` row.
/// The file's pin rows must be the compiled ones, so the hash describes this
/// builder.
fn pin(sources: &Path, lock_file: &Path, work_dir: &Path) -> Result<String, String> {
    let lock = lock()?;
    let text =
        fs::read_to_string(lock_file).map_err(|e| format!("read {}: {e}", lock_file.display()))?;
    if debian_root::parse(&text)?.pins != lock.pins {
        return Err(format!(
            "{} pins differ from the compiled lock: rebuild td-builder first",
            lock_file.display()
        ));
    }
    let work = Work::new(work_dir.join(format!(".{NAME}-work.{}", std::process::id())))?;
    let root = assemble(&lock, sources, &work.0)?;
    let tree = sandbox::nar_hash_of(&root).map_err(|e| format!("hash the assembled root: {e}"))?;
    let mut out: String = text
        .lines()
        .filter(|line| line.split_whitespace().next() != Some("tree"))
        .map(|line| format!("{line}\n"))
        .collect();
    out.push_str(&format!("tree {tree}\n"));
    let tmp = lock_file.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&tmp, out).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, lock_file).map_err(|e| format!("commit {}: {e}", lock_file.display()))?;
    Ok(tree)
}

/// Refuse an output built in the root that keeps the root at run time: a
/// reference to its store path, an ELF whose interpreter or RUNPATH names a
/// path outside the store (the root's `/lib64/ld-linux-x86-64.so.2`, its
/// `/usr/lib`), or an absolute symlink out of the store. It cannot see a
/// static object of the root's linked into an output; AGENTS.md says so.
pub(crate) fn check_output(
    root: &str,
    output: &str,
    refs: &[String],
    newstore: &Path,
) -> Result<(), String> {
    if refs.iter().any(|r| r == root) {
        return Err(format!("{output} references the build root {root}"));
    }
    let base = output
        .rsplit('/')
        .next()
        .ok_or_else(|| format!("{output}: not a store path"))?;
    let store_prefix = format!("{}/", store::store_dir());
    check_tree(&newstore.join(base), output, output, &store_prefix)
}

/// `path` with `.` and `..` resolved lexically, or None when `..` climbs
/// above `/` or the path is not absolute.
fn lexical(path: &str) -> Option<String> {
    let rest = path.strip_prefix('/')?;
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(format!("/{}", parts.join("/")))
}

/// Whether `path`, resolved lexically, names something under the store.
fn in_store(path: &str, store_prefix: &str) -> bool {
    lexical(path).is_some_and(|p| p.starts_with(store_prefix))
}

/// `path` on disk is `canonical` in the store; `..` and `$ORIGIN` are
/// resolved against `canonical` so neither climbs out unseen.
fn check_tree(
    path: &Path,
    canonical: &str,
    output: &str,
    store_prefix: &str,
) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| format!("stat {}: {e}", path.display()))?;
    let dir = canonical.rsplit_once('/').map_or("", |(dir, _)| dir);
    if meta.file_type().is_symlink() {
        let target =
            fs::read_link(path).map_err(|e| format!("read link {}: {e}", path.display()))?;
        let target = target.to_string_lossy();
        let resolved = if target.starts_with('/') {
            target.to_string()
        } else {
            format!("{dir}/{target}")
        };
        if !in_store(&resolved, store_prefix) {
            return Err(format!(
                "{output}: {} links to {target}, outside the store",
                path.display()
            ));
        }
        return Ok(());
    }
    if meta.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?
            .collect::<Result<_, _>>()
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let child = format!("{canonical}/{}", name.to_string_lossy());
            check_tree(&entry.path(), &child, output, store_prefix)?;
        }
        return Ok(());
    }
    if !meta.is_file() || !is_elf(path)? {
        return Ok(());
    }
    if let Some(interp) = crate::elf::read_interp(path)? {
        if !in_store(&interp, store_prefix) {
            return Err(format!(
                "{output}: {} names the interpreter {interp}, outside the store",
                path.display()
            ));
        }
    }
    if let Some(rpath) = crate::elf::read_rpath(path)? {
        let outside = rpath.split(':').find(|entry| {
            let resolved = match entry
                .strip_prefix("$ORIGIN")
                .or_else(|| entry.strip_prefix("${ORIGIN}"))
            {
                Some(rest) => format!("{dir}{rest}"),
                None => entry.to_string(),
            };
            !in_store(&resolved, store_prefix)
        });
        if let Some(entry) = outside {
            return Err(format!(
                "{output}: {} searches {entry}, outside the store",
                path.display()
            ));
        }
    }
    Ok(())
}

fn is_elf(path: &Path) -> Result<bool, String> {
    use std::io::Read;
    let mut magic = [0u8; 4];
    let mut file = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == *b"\x7fELF"),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// `td-builder build-root ...`.
pub(crate) fn run(args: &[String]) -> Result<(), String> {
    match args {
        [verb, sources, store, db] if verb == "assemble" => {
            let path = ensure(Path::new(sources), Path::new(store), Path::new(db))?;
            println!("{path}");
            Ok(())
        }
        [verb, sources, lock_file, work] if verb == "pin" => {
            let tree = pin(Path::new(sources), Path::new(lock_file), Path::new(work))?;
            println!("tree {tree}");
            Ok(())
        }
        [verb] if verb == "path" => {
            println!("{}", store_path(&lock()?)?);
            Ok(())
        }
        _ => Err(
            "usage: td-builder build-root assemble SOURCES SEED-STORE SEED-DB | \
                  pin SOURCES LOCK-FILE WORK | path"
                .into(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn tmp(tag: &str) -> crate::test_temp::TempDir {
        crate::test_temp::TempDir::new(&format!("td-build-root-{tag}")).unwrap()
    }

    fn ar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = b"!<arch>\n".to_vec();
        for (name, body) in members {
            let header = format!(
                "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                format!("{name}/"),
                0,
                0,
                0,
                100644,
                body.len()
            );
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(body);
            if body.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    #[test]
    fn the_deb_payload_is_its_data_tar_xz_member() {
        let deb = ar(&[
            ("debian-binary", b"2.0\n"),
            ("control.tar.xz", b"odd"),
            ("data.tar.xz", b"payload"),
        ]);
        assert_eq!(deb_data(&deb, "x.deb").unwrap(), b"payload");
        for (members, why) in [
            (
                ar(&[("debian-binary", b"2.0\n"), ("data.tar.zst", b"p")]),
                "td reads data.tar.xz only",
            ),
            (ar(&[("data.tar.xz", b"p")]), "precedes debian-binary"),
            (ar(&[("debian-binary", b"3.0\n")]), "not a 2.0 .deb"),
            (ar(&[("debian-binary", b"2.0\n")]), "no data.tar.xz member"),
        ] {
            let error = deb_data(&members, "x.deb").unwrap_err();
            assert!(error.contains(why), "{error}");
        }
        let mut truncated = ar(&[("debian-binary", b"2.0\n"), ("data.tar.xz", b"payload")]);
        truncated.truncate(truncated.len() - 3);
        assert!(deb_data(&truncated, "x.deb")
            .unwrap_err()
            .contains("truncated"));
        assert!(deb_data(b"not ar", "x.deb").is_err());
    }

    /// A root shaped like the image's: usrmerge links and real `usr` dirs.
    fn image_root(dir: &Path) -> PathBuf {
        let root = dir.join("root");
        for sub in ["usr/bin", "usr/lib", "usr/share"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        symlink("usr/bin", root.join("bin")).unwrap();
        symlink("usr/lib", root.join("lib")).unwrap();
        root
    }

    #[test]
    fn a_deb_merges_through_usrmerge_links_and_drops_excluded_paths() {
        let dir = tmp("merge");
        let root = image_root(dir.path());
        fs::write(root.join("usr/bin/old"), b"old").unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(src.join("bin")).unwrap();
        fs::create_dir_all(src.join("usr/share/doc/pkg")).unwrap();
        fs::create_dir_all(src.join("usr/share/pkg")).unwrap();
        fs::write(src.join("bin/tool"), b"tool").unwrap();
        fs::write(src.join("bin/old"), b"new").unwrap();
        fs::write(src.join("usr/share/doc/pkg/copyright"), b"c").unwrap();
        fs::write(src.join("usr/share/pkg/data"), b"d").unwrap();
        symlink("tool", src.join("bin/alias")).unwrap();

        merge(&src, &root, "").unwrap();

        assert_eq!(fs::read(root.join("usr/bin/tool")).unwrap(), b"tool");
        assert_eq!(fs::read(root.join("usr/bin/old")).unwrap(), b"new");
        assert_eq!(
            fs::read_link(root.join("usr/bin/alias")).unwrap(),
            Path::new("tool")
        );
        assert_eq!(fs::read(root.join("usr/share/pkg/data")).unwrap(), b"d");
        assert!(!root.join("usr/share/doc").exists());
        assert_eq!(
            fs::read_link(root.join("bin")).unwrap(),
            Path::new("usr/bin")
        );
    }

    #[test]
    fn a_merge_never_writes_through_another_link_or_swaps_a_kind() {
        let dir = tmp("conflict");
        let root = image_root(dir.path());
        fs::create_dir_all(root.join("usr/lib/real")).unwrap();
        symlink("real", root.join("usr/lib/linked")).unwrap();
        fs::write(root.join("usr/lib/file"), b"f").unwrap();
        let case = |name: &str, build: &dyn Fn(&Path)| {
            let src = dir.path().join(name);
            build(&src);
            merge(&src, &root, "").unwrap_err()
        };
        let through = case("through", &|src| {
            fs::create_dir_all(src.join("usr/lib/linked")).unwrap();
            fs::write(src.join("usr/lib/linked/x"), b"x").unwrap();
        });
        assert!(
            through.contains("a directory over a non-directory"),
            "{through}"
        );
        assert!(!root.join("usr/lib/real/x").exists());
        let dir_over_file = case("dir-over-file", &|src| {
            fs::create_dir_all(src.join("usr/lib/file")).unwrap();
        });
        assert!(dir_over_file.contains("a directory over a non-directory"));
        let file_over_dir = case("file-over-dir", &|src| {
            fs::create_dir_all(src.join("usr")).unwrap();
            fs::write(src.join("usr/lib"), b"x").unwrap();
        });
        assert!(file_over_dir.contains("a non-directory over a directory"));
    }

    #[test]
    fn a_whiteout_in_the_layer_is_refused() {
        let dir = tmp("whiteout");
        fs::create_dir_all(dir.path().join("etc")).unwrap();
        fs::write(dir.path().join("etc/.wh.gone"), b"").unwrap();
        assert!(refuse_whiteouts(dir.path())
            .unwrap_err()
            .contains("whiteout"));
        fs::remove_file(dir.path().join("etc/.wh.gone")).unwrap();
        refuse_whiteouts(dir.path()).unwrap();
    }

    #[test]
    fn alternatives_need_their_targets_and_keep_existing_links() {
        let dir = tmp("alternatives");
        let root = image_root(dir.path());
        for tool in [
            "c89-gcc",
            "c99-gcc",
            "gcc",
            "xz",
            "unxz",
            "bison.yacc",
            "cpp",
        ] {
            fs::write(root.join("usr/bin").join(tool), b"").unwrap();
        }
        symlink("gawk-not-gcc", root.join("usr/bin/cc")).unwrap();
        add_alternatives(&root).unwrap();
        assert_eq!(
            fs::read_link(root.join("usr/bin/yacc")).unwrap(),
            Path::new("bison.yacc")
        );
        assert_eq!(
            fs::read_link(root.join("usr/lib/cpp")).unwrap(),
            Path::new("../bin/cpp")
        );
        assert_eq!(
            fs::read_link(root.join("usr/bin/cc")).unwrap(),
            Path::new("gawk-not-gcc")
        );

        let bare = image_root(&dir.path().join("bare"));
        let error = add_alternatives(&bare).unwrap_err();
        assert!(error.contains("the target is absent"), "{error}");
    }

    #[test]
    fn the_skeleton_needs_directories_for_its_mount_points() {
        let dir = tmp("skeleton");
        let root = image_root(dir.path());
        add_skeleton(&root).unwrap();
        assert!(SKELETON.iter().all(|d| root.join(d).is_dir()));
        let bad = image_root(&dir.path().join("bad"));
        fs::write(bad.join("proc"), b"").unwrap();
        assert!(add_skeleton(&bad).unwrap_err().contains("proc"));
    }

    #[test]
    fn an_output_may_not_keep_the_root() {
        let dir = tmp("output");
        let store = store::store_dir();
        let root = format!("{store}/0123456789abcdfghijklmnpqrsvwxyz-{NAME}");
        let output = format!("{store}/0123456789abcdfghijklmnpqrsvwxyy-out-1");
        let base = output.rsplit('/').next().unwrap();
        let out = dir.path().join(base);
        fs::create_dir_all(out.join("bin")).unwrap();
        fs::write(out.join("bin/script"), b"#!/bin/sh\n").unwrap();
        check_output(&root, &output, &[], dir.path()).unwrap();

        let error = check_output(&root, &output, &[root.clone()], dir.path()).unwrap_err();
        assert!(error.contains("references the build root"), "{error}");

        let elf =
            |name: &str, bytes: Vec<u8>| fs::write(out.join("bin").join(name), bytes).unwrap();
        elf(
            "store-interp",
            crate::elf::tests::synth_interp_elf(&format!("{store}/x/ld.so"), true),
        );
        check_output(&root, &output, &[], dir.path()).unwrap();
        elf(
            "debian",
            crate::elf::tests::synth_interp_elf("/lib64/ld-linux-x86-64.so.2", true),
        );
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(error.contains("outside the store"), "{error}");
        fs::remove_file(out.join("bin/debian")).unwrap();

        elf(
            "origin",
            crate::elf::tests::synth_dyn_elf("$ORIGIN/../lib", true, true),
        );
        check_output(&root, &output, &[], dir.path()).unwrap();
        elf(
            "climbs",
            crate::elf::tests::synth_dyn_elf("$ORIGIN/../../../usr/lib", true, true),
        );
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(
            error.contains("searches $ORIGIN/../../../usr/lib"),
            "{error}"
        );
        fs::remove_file(out.join("bin/climbs")).unwrap();
        elf(
            "usr-lib",
            crate::elf::tests::synth_dyn_elf("$ORIGIN:/usr/lib", true, true),
        );
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(error.contains("searches /usr/lib"), "{error}");
    }

    #[test]
    fn the_compiled_lock_names_a_pinned_input_addressed_root() {
        let lock = lock().unwrap();
        assert!(lock.tree.is_some(), "the committed lock pins its tree");
        let path = store_path(&lock).unwrap();
        assert!(path.ends_with(&format!("-{NAME}")), "{path}");
        let mut moved = lock.clone();
        moved.pins.swap(1, 2);
        assert_ne!(
            store_path(&moved).unwrap(),
            path,
            "the pin order is part of the key"
        );
        let mut retree = lock.clone();
        retree.tree = Some(format!("sha256:{}", "0".repeat(64)));
        assert_ne!(store_path(&retree).unwrap(), path, "so is the pinned tree");
        assert_ne!(
            db_path(Path::new("/x/seed.db"), &retree).unwrap(),
            db_path(Path::new("/x/seed.db"), &lock).unwrap()
        );
        retree.tree = None;
        assert!(store_path(&retree).unwrap_err().contains("no `tree' row"));
        let db = db_path(Path::new("/x/seed.db"), &lock).unwrap();
        assert_eq!(db.parent(), Some(Path::new("/x")));
    }

    fn db_rows(out: &Path, rows: &[(&str, &str)]) {
        use store_db::{Table, Value};
        let rows = rows
            .iter()
            .enumerate()
            .map(|(n, (path, hash))| {
                (
                    i64::try_from(n + 1).unwrap(),
                    vec![
                        Value::Null,
                        Value::Text(path.to_string()),
                        Value::Text(hash.to_string()),
                        Value::Int(1),
                        Value::Null,
                        Value::Int(1),
                    ],
                )
            })
            .collect();
        let tables = [
            Table {
                name: "ValidPaths",
                sql: "CREATE TABLE ValidPaths (id integer primary key, path text, hash text, registrationTime integer, deriver text, narSize integer)",
                rows,
            },
            Table {
                name: "Refs",
                sql: "CREATE TABLE Refs (referrer integer, reference integer)",
                rows: Vec::new(),
            },
        ];
        fs::write(out, store_db::write_db(&tables)).unwrap();
    }

    #[test]
    fn a_root_db_must_register_exactly_the_pinned_root_at_its_bytes() {
        let dir = tmp("db");
        let path = format!(
            "{}/0123456789abcdfghijklmnpqrsvwxyz-{NAME}",
            store::store_dir()
        );
        let base = path.rsplit('/').next().unwrap();
        let items = dir.path().join("items");
        let tree_dir = items.join(base);
        fs::create_dir_all(&tree_dir).unwrap();
        fs::write(tree_dir.join("file"), b"the root").unwrap();
        let tree = sandbox::nar_hash_of(&tree_dir).unwrap();
        let db = dir.path().join("root.db");

        write_db(&path, &tree_dir, &db).unwrap();
        authenticate_db_with(&path, &tree, &db, &items).unwrap();

        let other = format!(
            "{}/0123456789abcdfghijklmnpqrsvwxyy-{NAME}",
            store::store_dir()
        );
        for (rows, why) in [
            (
                vec![
                    (path.as_str(), tree.as_str()),
                    (other.as_str(), tree.as_str()),
                ],
                "two rows",
            ),
            (vec![(other.as_str(), tree.as_str())], "another path"),
            (vec![(path.as_str(), "sha256:00")], "another hash"),
        ] {
            db_rows(&db, &rows);
            let error = authenticate_db_with(&path, &tree, &db, &items).unwrap_err();
            assert!(error.contains("at the pinned"), "{why}: {error}");
        }

        // The row is right but the bytes on disk are not.
        db_rows(&db, &[(path.as_str(), tree.as_str())]);
        authenticate_db_with(&path, &tree, &db, &items).unwrap();
        fs::write(tree_dir.join("file"), b"swapped").unwrap();
        let error = authenticate_db_with(&path, &tree, &db, &items).unwrap_err();
        assert!(error.contains("on disk hashes"), "{error}");

        let error =
            authenticate_db_with(&path, &tree, &dir.path().join("absent.db"), &items).unwrap_err();
        assert!(error.contains("build-root assemble"), "{error}");
    }

    #[test]
    fn an_output_may_not_link_out_of_the_store() {
        let dir = tmp("links");
        let store = store::store_dir();
        let root = format!("{store}/0123456789abcdfghijklmnpqrsvwxyz-{NAME}");
        let output = format!("{store}/0123456789abcdfghijklmnpqrsvwxyy-out-1");
        let out = dir.path().join(output.rsplit('/').next().unwrap());
        fs::create_dir_all(out.join("bin")).unwrap();
        symlink("../lib/x", out.join("bin/relative")).unwrap();
        symlink(format!("{store}/abc-dep/bin/x"), out.join("bin/store")).unwrap();
        check_output(&root, &output, &[], dir.path()).unwrap();
        symlink("../../../usr/lib/x", out.join("bin/climbs")).unwrap();
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(
            error.contains("links to ../../../usr/lib/x, outside the store"),
            "{error}"
        );
        fs::remove_file(out.join("bin/climbs")).unwrap();
        symlink(format!("{store}/../usr/bin/gcc"), out.join("bin/cc")).unwrap();
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(error.contains("outside the store"), "{error}");
        fs::remove_file(out.join("bin/cc")).unwrap();
        symlink("/usr/bin/gcc", out.join("bin/cc")).unwrap();
        let error = check_output(&root, &output, &[], dir.path()).unwrap_err();
        assert!(
            error.contains("links to /usr/bin/gcc, outside the store"),
            "{error}"
        );
    }

    #[test]
    fn paths_resolve_lexically_before_the_store_test() {
        assert_eq!(
            lexical("/td/store/a/../b/./c").as_deref(),
            Some("/td/store/b/c")
        );
        assert_eq!(lexical("/.."), None);
        assert_eq!(lexical("relative"), None);
        assert!(in_store("/td/store/x/../y", "/td/store/"));
        assert!(!in_store("/td/store/../usr", "/td/store/"));
        assert!(!in_store("/td/store", "/td/store/"));
    }
}
