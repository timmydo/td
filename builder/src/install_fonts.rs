//! `td-builder install-fonts` — the pinned outline face installed for this
//! host's user, in the directory td-ui looks in after the image's
//! (td-ui/DESIGN.md, "Delivery and trust position"). The repository-root
//! `./install-fonts` entry script execs this.
//!
//! A development fixture, as `install-apps` is: a td program run on a host has
//! no `/etc/fonts/jetbrains-mono-nerd` and draws with Unifont until this
//! has run. What it installs is the font recipe's output, from the plan
//! `td-recipe-eval install-fonts-plan` prints from the recipe's own pins
//! and file lists: each pin is verified by SHA-256 before it is read, and
//! the archive is unpacked by the reader the recipe's `unpack` step uses.
//! A pin already in the shared sources cache is not fetched; a cold one is
//! fetched by the checkout's td-net, as `td-feed warm` fetches, verified
//! again and left in the cache. Nothing built or installed here enters a
//! build, so the host's own cargo serves.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

#[derive(Debug, PartialEq)]
struct Pin {
    url: String,
    sha256: String,
    /// The pin's file name in the sources cache.
    file: String,
}

#[derive(Debug, PartialEq)]
struct Plan {
    /// The face's directory under a data directory.
    dir: String,
    archive: Pin,
    /// Files copied from the unpacked archive, by name.
    members: Vec<String>,
    /// Each notice's pin and its path under the face's directory.
    notices: Vec<(Pin, String)>,
}

impl Plan {
    fn pins(&self) -> impl Iterator<Item = &Pin> {
        std::iter::once(&self.archive).chain(self.notices.iter().map(|(pin, _)| pin))
    }
}

/// One plain file name: no separator, not `.` or `..`, not empty.
fn plain(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// A relative path of plain names.
fn relative(path: &str) -> bool {
    !path.starts_with('/') && path.split('/').all(plain)
}

fn pin(url: &str, sha256: &str, file: &str) -> Result<Pin, String> {
    if !url.starts_with("https://") {
        return Err(format!("pin URL {url} is not https"));
    }
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("pin digest {sha256} is not a SHA-256"));
    }
    if !plain(file) {
        return Err(format!("pin file {file} is not a plain file name"));
    }
    Ok(Pin {
        url: url.into(),
        sha256: sha256.into(),
        file: file.into(),
    })
}

/// `install-fonts-plan`'s lines: one directory, one archive, at least one
/// member, each field checked before anything names a path by it.
fn parse_plan(text: &str) -> Result<Plan, String> {
    let mut dir = None;
    let mut archive = None;
    let mut members = Vec::new();
    let mut notices = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["dir", path] if dir.is_none() && relative(path) => dir = Some((*path).to_string()),
            ["archive", url, sha256, file] if archive.is_none() => {
                archive = Some(pin(url, sha256, file)?)
            }
            ["member", name] if plain(name) => members.push((*name).to_string()),
            ["notice", url, sha256, file, path] if relative(path) => {
                notices.push((pin(url, sha256, file)?, (*path).to_string()))
            }
            _ => return Err(format!("unexpected install-fonts-plan line {line:?}")),
        }
    }
    let dir = dir.ok_or("install-fonts-plan named no directory")?;
    let archive = archive.ok_or("install-fonts-plan named no archive")?;
    if members.is_empty() {
        return Err("install-fonts-plan named no member".into());
    }
    Ok(Plan {
        dir,
        archive,
        members,
        notices,
    })
}

/// The user's data directory the plan's directory goes under, as td-ui's
/// search names it: `$XDG_DATA_HOME`, else `$HOME/.local/share`. A
/// relative value is ignored, as the XDG base directory specification
/// says.
fn data_home(home: Option<&OsStr>, data_home: Option<&OsStr>) -> Result<PathBuf, String> {
    fn absolute(value: Option<&OsStr>) -> Option<&Path> {
        value.map(Path::new).filter(|p| p.is_absolute())
    }
    if let Some(data_home) = absolute(data_home) {
        return Ok(data_home.to_path_buf());
    }
    absolute(home)
        .map(|home| home.join(".local/share"))
        .ok_or_else(|| "neither XDG_DATA_HOME nor HOME is an absolute path".into())
}

fn verified(path: &Path, sha256: &str) -> bool {
    td_engine::sha256::sha256_file(path).is_ok_and(|have| have == sha256)
}

/// A fresh hidden directory of this process's beside `path`, named after
/// it and `tag`. Those an install that was killed left, named for a
/// process no longer running, are removed first, when `/proc` is there to
/// say which are running, as `td-net launch` sweeps its runtime directories.
/// `/proc` answers for this process's pid namespace, so two installs at
/// once from namespaces that share the home (a container beside the host)
/// can take each other's; a rerun repairs what that breaks.
fn sibling(path: &Path, tag: &str) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    let mut prefix = std::ffi::OsString::from(".");
    prefix.push(name);
    prefix.push(format!(".{tag}-"));
    let parent = path.parent().unwrap_or(Path::new("."));
    sweep(parent, prefix.as_encoded_bytes(), Path::new("/proc"));
    let mut hidden = prefix;
    hidden.push(std::process::id().to_string());
    let dir = path.with_file_name(hidden);
    if fs::symlink_metadata(&dir).is_ok() {
        fs::remove_dir_all(&dir).map_err(|e| format!("clear {}: {e}", dir.display()))?;
    }
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Remove `parent`'s entries, directories or files, named `prefix` and a
/// process id that `proc` does not list; nothing when `proc` is not there
/// to ask.
pub(crate) fn sweep(parent: &Path, prefix: &[u8], proc: &Path) {
    if !proc.join("self").is_dir() {
        return;
    }
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .as_encoded_bytes()
            .strip_prefix(prefix)
            .and_then(|pid| std::str::from_utf8(pid).ok())
            .filter(|pid| pid.parse::<u32>().is_ok());
        if let Some(pid) = pid {
            if !proc.join(pid).is_dir() {
                let path = entry.path();
                let _ = match entry.file_type() {
                    Ok(kind) if kind.is_dir() => fs::remove_dir_all(path),
                    _ => fs::remove_file(path),
                };
            }
        }
    }
}

/// Fetch the cold pins with `td_net` into a scratch store in `cache`, then
/// move each into `cache` once its digest is the pin's.
fn fetch(td_net: &Path, root: &Path, cache: &Path, cold: &[&Pin]) -> Result<(), String> {
    let scratch = sibling(&cache.join("install-fonts"), "fetch")?;
    let outcome = (|| {
        let index = scratch.join("index");
        let store = scratch.join("store");
        let text: String = cold
            .iter()
            .map(|pin| format!("{} {} {}\n", pin.file, pin.url, pin.sha256))
            .collect();
        fs::write(&index, text).map_err(|e| format!("write {}: {e}", index.display()))?;
        let status = Command::new(td_net)
            .args(["feed", "warm", "index"])
            .arg(&index)
            .arg(&store)
            .current_dir(root)
            .stdin(Stdio::null())
            .status()
            .map_err(|e| format!("cannot run {}: {e}", td_net.display()))?;
        if !status.success() {
            return Err(format!("td-net feed warm index failed ({status})"));
        }
        for pin in cold {
            let fetched = store.join(&pin.file);
            if !verified(&fetched, &pin.sha256) {
                return Err(format!("{} is not {}", pin.url, pin.sha256));
            }
            let cached = cache.join(&pin.file);
            fs::rename(&fetched, &cached)
                .map_err(|e| format!("move {} -> {}: {e}", fetched.display(), cached.display()))?;
        }
        Ok(())
    })();
    let _ = fs::remove_dir_all(&scratch);
    outcome
}

/// Copy `from` to `to` as a 0644 regular file, `to`'s directory made.
fn place(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(dir) = to.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    fs::copy(from, to).map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("chmod {}: {e}", to.display()))
}

/// The plan's files from `cache` into `dest`: every pin verified, the
/// archive unpacked beside the cache, its members (regular files, never
/// links) and the notices staged beside `dest`, and the staged directory
/// put in `dest`'s place, the one it replaces removed after; the face is
/// installed once that swap is made, so a copy that cannot be removed is
/// named on standard error rather than failing the install.
fn install(cache: &Path, plan: &Plan, dest: &Path) -> Result<(), String> {
    for pin in plan.pins() {
        let cached = cache.join(&pin.file);
        if !verified(&cached, &pin.sha256) {
            return Err(format!("{} is not {}", cached.display(), pin.sha256));
        }
    }
    let unpacked = sibling(&cache.join("install-fonts"), "unpack")?;
    let stage = sibling(dest, "stage");
    let staged = stage.and_then(|stage| {
        let outcome = (|| {
            crate::tar::unpack_archive(&cache.join(&plan.archive.file), &unpacked, true)?;
            for name in &plan.members {
                let member = unpacked.join(name);
                let regular = fs::symlink_metadata(&member).is_ok_and(|m| m.is_file());
                if !regular {
                    return Err(format!("{} has no regular file {name}", plan.archive.file));
                }
                place(&member, &stage.join(name))?;
            }
            for (pin, path) in &plan.notices {
                place(&cache.join(&pin.file), &stage.join(path))?;
            }
            fs::set_permissions(&stage, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {}: {e}", stage.display()))
        })();
        if outcome.is_err() {
            let _ = fs::remove_dir_all(&stage);
        }
        outcome.map(|()| stage)
    });
    let _ = fs::remove_dir_all(&unpacked);
    let stage = staged?;
    let old = if fs::symlink_metadata(dest).is_ok() {
        let old = sibling(dest, "old")?;
        fs::remove_dir(&old).map_err(|e| format!("clear {}: {e}", old.display()))?;
        fs::rename(dest, &old)
            .map_err(|e| format!("move {} -> {}: {e}", dest.display(), old.display()))?;
        Some(old)
    } else {
        None
    };
    if let Err(e) = fs::rename(&stage, dest) {
        if let Some(old) = &old {
            let _ = fs::rename(old, dest);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!(
            "move {} -> {}: {e}",
            stage.display(),
            dest.display()
        ));
    }
    if let Some(old) = old {
        if let Err(e) = fs::remove_dir_all(&old) {
            eprintln!(
                "install-fonts: the replaced copy is left at {}: {e}",
                old.display()
            );
        }
    }
    Ok(())
}

fn install_fonts(root: &Path) -> Result<PathBuf, String> {
    for dir in ["recipes", "net"] {
        if !root.join(dir).join("Cargo.toml").is_file() {
            return Err(format!(
                "{} is not the td checkout ({dir}/Cargo.toml is not under it): run \
                 ./install-fonts from the repository root",
                root.display()
            ));
        }
    }
    let data = data_home(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("XDG_DATA_HOME").as_deref(),
    )?;
    let tools = crate::host_run::tools(root)?;
    let eval = crate::host_run::build(root, &tools, "recipes", "td-recipe-eval")?;
    let output = Command::new(&eval)
        .arg("install-fonts-plan")
        .current_dir(root)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", eval.display()))?;
    if !output.status.success() {
        return Err(format!(
            "td-recipe-eval install-fonts-plan failed ({})",
            output.status
        ));
    }
    let plan = parse_plan(&String::from_utf8_lossy(&output.stdout))?;
    let dest = data.join(&plan.dir);
    let cache = crate::bootstrap::shared_sources_dir();
    fs::create_dir_all(&cache).map_err(|e| format!("create {}: {e}", cache.display()))?;
    let mut cold: Vec<&Pin> = plan
        .pins()
        .filter(|pin| !verified(&cache.join(&pin.file), &pin.sha256))
        .collect();
    // Two notices may share a pin: each file is fetched and moved once.
    cold.sort_by(|a, b| a.file.cmp(&b.file));
    cold.dedup_by(|a, b| a.file == b.file);
    if !cold.is_empty() {
        eprintln!("install-fonts: fetching {} pinned files", cold.len());
        let td_net = crate::host_run::build(root, &tools, "net", "td-net")?;
        fetch(&td_net, root, &cache, &cold)?;
    }
    install(&cache, &plan, &dest)?;
    Ok(dest)
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: td-builder install-fonts");
        return ExitCode::from(2);
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("td-builder: install-fonts: getcwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match install_fonts(&root) {
        Ok(dest) => {
            eprintln!(
                "install-fonts: the JetBrains Mono Nerd Font is in {}",
                dest.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("td-builder: install-fonts: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("td-install-fonts-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // An uncompressed ustar archive of regular files and, for a member
    // named with a trailing `@`, a symbolic link to `/etc/passwd`.
    fn ustar(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, body) in members {
            let (path, link) = match path.strip_suffix('@') {
                Some(path) => (path, true),
                None => (*path, false),
            };
            let body: &[u8] = if link { b"" } else { body };
            let mut h = [0u8; 512];
            let put = |h: &mut [u8; 512], off: usize, s: &[u8]| {
                h[off..off + s.len()].copy_from_slice(s);
            };
            put(&mut h, 0, path.as_bytes());
            put(&mut h, 100, b"0000644\0");
            put(&mut h, 108, b"0000000\0");
            put(&mut h, 116, b"0000000\0");
            put(&mut h, 124, format!("{:011o}\0", body.len()).as_bytes());
            put(&mut h, 136, b"00000000000\0");
            h[156] = if link { b'2' } else { b'0' };
            if link {
                put(&mut h, 157, b"/etc/passwd");
            }
            put(&mut h, 257, b"ustar\0");
            put(&mut h, 263, b"00");
            h[148..156].fill(b' ');
            let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
            put(&mut h, 148, format!("{sum:06o}\0 ").as_bytes());
            out.extend_from_slice(&h);
            out.extend_from_slice(body);
            out.extend(std::iter::repeat_n(0u8, (512 - body.len() % 512) % 512));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    fn cached(cache: &Path, file: &str, bytes: &[u8]) -> Pin {
        fs::write(cache.join(file), bytes).unwrap();
        Pin {
            url: format!("https://example.org/{file}"),
            sha256: td_engine::sha256::hex_digest(bytes),
            file: file.into(),
        }
    }

    fn plan(cache: &Path, archive: &[(&str, &[u8])], members: &[&str]) -> Plan {
        Plan {
            dir: "fonts/face".into(),
            archive: cached(cache, "face.tar", &ustar(archive)),
            members: members.iter().map(|m| m.to_string()).collect(),
            notices: vec![(
                cached(cache, "set-LICENSE", b"licence"),
                "licenses/set/LICENSE".into(),
            )],
        }
    }

    fn tree(dir: &Path) -> Vec<(String, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(at) = stack.pop() {
            for entry in fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let name = path.strip_prefix(dir).unwrap().display().to_string();
                if meta.is_dir() {
                    stack.push(path);
                } else {
                    let mode = meta.permissions().mode() & 0o7777;
                    out.push((name, mode, fs::read(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    #[test]
    fn the_plan_is_read_field_by_field() {
        let sha = "a".repeat(64);
        let text = format!(
            "dir\tfonts/face\narchive\thttps://h/a.tar.xz\t{sha}\ta.tar.xz\n\
             member\tR.ttf\nnotice\thttps://h/L\t{sha}\tset-L\tlicenses/set/L\n"
        );
        let plan = parse_plan(&text).unwrap();
        assert_eq!(plan.dir, "fonts/face");
        assert_eq!(
            plan.archive,
            pin("https://h/a.tar.xz", &sha, "a.tar.xz").unwrap()
        );
        assert_eq!(plan.members, ["R.ttf"]);
        assert_eq!(plan.notices.len(), 1);
        assert_eq!(plan.notices[0].1, "licenses/set/L");
        for bad in [
            format!("dir\tfonts/face\narchive\thttp://h/a\t{sha}\ta"),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{}\ta", "A".repeat(64)),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\t../a"),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\ta\nmember\t../R.ttf"),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\ta\nmember\tdir/R.ttf"),
            format!(
                "archive\thttps://h/a\t{sha}\ta\nmember\tR\nnotice\thttps://h/L\t{sha}\tL\t/abs"
            ),
            format!(
                "archive\thttps://h/a\t{sha}\ta\nmember\tR\nnotice\thttps://h/L\t{sha}\tL\ta/../b"
            ),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\ta\narchive\thttps://h/a\t{sha}\ta\nmember\tR"),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\ta"),
            "dir\tfonts/face\nmember\tR".to_string(),
            format!("archive\thttps://h/a\t{sha}\ta\nmember\tR"),
            format!("dir\t/fonts\narchive\thttps://h/a\t{sha}\ta\nmember\tR"),
            format!("dir\tfonts/../x\narchive\thttps://h/a\t{sha}\ta\nmember\tR"),
            format!("dir\ta\ndir\tb\narchive\thttps://h/a\t{sha}\ta\nmember\tR"),
            format!("dir\tfonts/face\narchive\thttps://h/a\t{sha}\ta\nmember\tR\nextra"),
        ] {
            assert!(parse_plan(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_face_goes_under_the_xdg_data_home() {
        let dir = |home: Option<&str>, data: Option<&str>| {
            data_home(home.map(OsStr::new), data.map(OsStr::new))
        };
        assert_eq!(dir(Some("/h"), None).unwrap(), Path::new("/h/.local/share"));
        assert_eq!(dir(Some("/h"), Some("/d")).unwrap(), Path::new("/d"));
        assert_eq!(
            dir(Some("/h"), Some("rel")).unwrap(),
            Path::new("/h/.local/share")
        );
        assert_eq!(
            dir(Some("/h"), Some("")).unwrap(),
            Path::new("/h/.local/share")
        );
        assert!(dir(None, None).is_err());
        assert!(dir(Some("rel"), None).is_err());
    }

    // A td-net that serves `feed warm index INDEX STORE` from `served`:
    // each index line's file copied into the store, as td-net fetches it.
    fn fake_td_net(base: &Path, served: &Path) -> PathBuf {
        let path = base.join("td-net");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\n\
                 [ \"$1 $2 $3\" = 'feed warm index' ] && [ $# = 5 ] || exit 9\n\
                 mkdir -p \"$5\"\n\
                 while read -r file url sha; do\n\
                 \tcp '{}'/\"$file\" \"$5/$file\" || exit 8\n\
                 done < \"$4\"\n",
                served.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn a_fetched_pin_enters_the_cache_only_with_its_digest() {
        let base = scratch("fetch");
        let (cache, served) = (base.join("cache"), base.join("served"));
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(&served).unwrap();
        let good = cached(&served, "face.tar", b"archive");
        let notice = cached(&served, "set-LICENSE", b"licence");
        let td_net = fake_td_net(&base, &served);
        fetch(&td_net, &base, &cache, &[&good, &notice]).unwrap();
        assert_eq!(fs::read(cache.join("face.tar")).unwrap(), b"archive");
        assert_eq!(fs::read(cache.join("set-LICENSE")).unwrap(), b"licence");
        let mut names: Vec<String> = fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["face.tar", "set-LICENSE"], "no scratch is left");

        // Served bytes that are not the pin's are refused and not cached.
        fs::remove_file(cache.join("face.tar")).unwrap();
        fs::write(served.join("face.tar"), b"altered").unwrap();
        let why = fetch(&td_net, &base, &cache, &[&good]).unwrap_err();
        assert!(why.contains(&good.url), "{why}");
        assert!(!cache.join("face.tar").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_killed_installs_leftovers_are_swept_and_a_running_ones_kept() {
        let base = scratch("sweep");
        let proc = base.join("proc");
        fs::create_dir_all(proc.join("self")).unwrap();
        fs::create_dir_all(proc.join("41")).unwrap();
        let fonts = base.join("fonts");
        for name in [
            ".face.stage-41",
            ".face.stage-42",
            ".face.stage-x",
            ".face.old-42",
            "face",
        ] {
            fs::create_dir_all(fonts.join(name)).unwrap();
        }
        sweep(&fonts, b".face.stage-", &proc);
        let mut left: Vec<String> = fs::read_dir(&fonts)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [".face.old-42", ".face.stage-41", ".face.stage-x", "face"]
        );
        // Without /proc nothing is removed.
        sweep(&fonts, b".face.old-", &base.join("none"));
        assert!(fonts.join(".face.old-42").is_dir());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_members_and_notices_replace_the_installed_directory() {
        let base = scratch("install");
        let cache = base.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let dest = base.join("data/fonts/face");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("stale.ttf"), b"old").unwrap();
        let plan = plan(
            &cache,
            &[
                ("R.ttf", b"regular"),
                ("B.ttf", b"bold"),
                ("other.ttf", b"x"),
            ],
            &["R.ttf", "B.ttf"],
        );
        install(&cache, &plan, &dest).unwrap();
        assert_eq!(
            tree(&dest),
            [
                ("B.ttf".to_string(), 0o644, b"bold".to_vec()),
                ("R.ttf".to_string(), 0o644, b"regular".to_vec()),
                (
                    "licenses/set/LICENSE".to_string(),
                    0o644,
                    b"licence".to_vec()
                ),
            ]
        );
        // Only the face's directory is left: no stage, old tree or unpack.
        let names = |dir: &Path| -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        assert_eq!(names(&base.join("data/fonts")), ["face"]);
        assert_eq!(names(&cache), ["face.tar", "set-LICENSE"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_pin_not_its_digest_or_a_member_not_a_file_leaves_the_face_alone() {
        let base = scratch("refuse");
        let cache = base.join("cache");
        fs::create_dir_all(&cache).unwrap();
        let dest = base.join("fonts/face");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("R.ttf"), b"kept").unwrap();
        let before = tree(&dest);

        let altered = plan(&cache, &[("R.ttf", b"regular")], &["R.ttf"]);
        fs::write(cache.join("set-LICENSE"), b"altered").unwrap();
        let why = install(&cache, &altered, &dest).unwrap_err();
        assert!(why.contains("set-LICENSE"), "{why}");

        for (archive, members) in [
            (&[("R.ttf", &b"regular"[..])][..], &["B.ttf"][..]),
            (&[("R.ttf@", &b""[..])][..], &["R.ttf"][..]),
        ] {
            let plan = plan(&cache, archive, members);
            let why = install(&cache, &plan, &dest).unwrap_err();
            assert!(why.contains("has no regular file"), "{why}");
        }
        assert_eq!(tree(&dest), before);
        let mut left: Vec<String> = fs::read_dir(base.join("fonts"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["face"]);
        fs::remove_dir_all(&base).unwrap();
    }
}
