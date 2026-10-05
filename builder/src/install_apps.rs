//! `td-builder install-apps` — the checkout's desktop programs built in
//! release mode with the host's cargo and installed for this host's user.
//! The repository-root `./install-apps` entry script execs this.
//!
//! td-dua, td-editor, td-pass, td-photo, td-pinentry, td-review,
//! td-taskmgr and td-term are installed in `~/.local/bin`. td-news,
//! td-mail and td-agent fetch only through td's fetch service, which a host
//! session does not run, so they are installed with td-net in
//! `~/.local/lib/td`, and their names in `~/.local/bin` are links to
//! td-net, whose `launch` applet serves a fetch service of their own and
//! runs the program of the link's name beside it (net/src/launch.rs).
//! td-net names those programs itself (`td-net launch --names`), and the
//! companions its launch gives them (`td-net launch --companions`): td-jail
//! and td-txt, installed beside them and linked nowhere, which the launch
//! names to td-agent as its workspace jail and the td-txt its tools run
//! (APPLICATIONS.md §X.7). td-net is therefore built first and asked.
//!
//! A development fixture, not host mode's jail: every program is built before
//! any is installed, so a failed build leaves the installed set as it was;
//! the launched programs and td-net are placed before the links to them;
//! and each file and link is put in place by a rename, so a program running
//! from there keeps its own file. Nothing built or installed here enters a
//! build.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

/// The programs installed, each its crate's directory and its binary's
/// name.
const APPS: &[&str] = &[
    "td-agent",
    "td-dua",
    "td-editor",
    "td-mail",
    "td-news",
    "td-pass",
    "td-photo",
    "td-pinentry",
    "td-review",
    "td-taskmgr",
    "td-term",
];
/// The fetch service and launcher, the checkout's `net` crate.
const TD_NET: &str = "td-net";
/// Where the launched programs and td-net go, under `~/.local`.
const LIB: &str = "lib/td";

/// `~/.local`, given `HOME`, which must be absolute.
fn local_dir(home: Option<&OsStr>) -> Result<PathBuf, String> {
    home.map(Path::new)
        .filter(|home| home.is_absolute())
        .map(|home| home.join(".local"))
        .ok_or_else(|| "HOME is not an absolute path".to_string())
}

/// `dest`, made if it is missing and proven writable by a file created
/// and removed in it, so an unwritable one is refused before anything is
/// built rather than after.
fn writable(dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    crate::install_fonts::sweep(dest, b".install-apps.probe-", Path::new("/proc"));
    let probe = dest.join(format!(".install-apps.probe-{}", std::process::id()));
    // Created exclusively: whatever is already at the name, a link among
    // them, is refused rather than followed and truncated.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| format!("{} is not writable: {e}", dest.display()))?;
    fs::remove_file(&probe).map_err(|e| format!("remove {}: {e}", probe.display()))
}

/// What is placed at a name: a copy of a built program, or a link.
#[derive(Debug, PartialEq)]
enum Source {
    File(PathBuf),
    Link(PathBuf),
}

/// One name to place in one directory.
#[derive(Debug, PartialEq)]
struct Item {
    dir: PathBuf,
    name: String,
    source: Source,
}

/// What `built` (each program's name and the file cargo built) becomes
/// under `local`: td-net and each program it `launched` in `local/lib/td`
/// with a link to td-net at the program's name in `local/bin`, placed
/// after them; td-net's `companions` in `local/lib/td` with no link; every
/// other program in `local/bin`. The link's target is
/// td-net's absolute path, since a relative one would resolve from
/// wherever a linked `local/bin` really is.
fn plan(
    built: &[(&str, PathBuf)],
    launched: &[String],
    companions: &[String],
    local: &Path,
) -> Result<Vec<Item>, String> {
    let (bin, lib) = (local.join("bin"), local.join(LIB));
    let td_net = lib.join(TD_NET);
    let launches = |name: &str| launched.iter().any(|l| l == name);
    let mut lib_items = Vec::new();
    let mut bin_items = Vec::new();
    for (name, from) in built {
        let file = Source::File(from.clone());
        if *name == TD_NET || launches(name) || companions.iter().any(|c| c == name) {
            lib_items.push(Item {
                dir: lib.clone(),
                name: (*name).to_string(),
                source: file,
            });
        } else {
            bin_items.push(Item {
                dir: bin.clone(),
                name: (*name).to_string(),
                source: file,
            });
        }
        if launches(name) {
            bin_items.push(Item {
                dir: bin.clone(),
                name: (*name).to_string(),
                source: Source::Link(td_net.clone()),
            });
        }
    }
    let unbuilt: Vec<&String> = launched
        .iter()
        .chain(companions)
        .filter(|l| !built.iter().any(|(name, _)| name == l))
        .collect();
    if !unbuilt.is_empty() {
        return Err(format!("td-net names programs not built here: {unbuilt:?}"));
    }
    if !lib_items.is_empty() && !built.iter().any(|(name, _)| *name == TD_NET) {
        return Err("a launched program needs td-net beside it".into());
    }
    lib_items.extend(bin_items);
    Ok(lib_items)
}

/// Stage `source` at `staged`, created exclusively so a live install in
/// another pid namespace with this pid staging the same name is refused,
/// never truncated or removed: a program copied mode 0755 with its write
/// handle closed before it is renamed, so it can be run at once; a link
/// made as it is.
fn stage(source: &Source, staged: &Path) -> Result<(), String> {
    stage_at(source, staged).map_err(|e| {
        if staged.symlink_metadata().is_ok() {
            format!(
                "{e}: another install with this pid holds it, or an install that \
                 was killed left it under the pid this one reuses; a rerun clears it"
            )
        } else {
            e
        }
    })
}

fn stage_at(source: &Source, staged: &Path) -> Result<(), String> {
    match source {
        Source::File(from) => {
            let target = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o755)
                .open(staged)
                .map_err(|e| format!("stage {}: {e}", staged.display()))?;
            let copied = fs::File::open(from)
                .and_then(|mut source| std::io::copy(&mut source, &mut &target));
            drop(target);
            copied
                .map_err(|e| format!("copy {} -> {}: {e}", from.display(), staged.display()))
                .and_then(|_| {
                    fs::set_permissions(staged, fs::Permissions::from_mode(0o755))
                        .map_err(|e| format!("chmod {}: {e}", staged.display()))
                })
                .inspect_err(|_| {
                    let _ = fs::remove_file(staged);
                })
        }
        Source::Link(target) => std::os::unix::fs::symlink(target, staged)
            .map_err(|e| format!("stage {}: {e}", staged.display())),
    }
}

/// Place each item in order: staged beside its name under a hidden name
/// of this process's, then renamed over whatever is there, and `placed`
/// told. A staged name an install that was killed left, named for a
/// process no longer running, is removed first. An error names what was
/// already placed, since those stay new.
fn install(items: &[Item], placed: &mut dyn FnMut(&Item)) -> Result<Vec<PathBuf>, String> {
    let mut installed: Vec<PathBuf> = Vec::new();
    for item in items {
        let failed = |e: String, installed: &[PathBuf]| {
            if installed.is_empty() {
                e
            } else {
                already(e, installed)
            }
        };
        fs::create_dir_all(&item.dir)
            .map_err(|e| failed(format!("create {}: {e}", item.dir.display()), &installed))?;
        let to = item.dir.join(&item.name);
        let prefix = format!(".{}.install-", item.name);
        crate::install_fonts::sweep(&item.dir, prefix.as_bytes(), Path::new("/proc"));
        let staged = item.dir.join(format!("{prefix}{}", std::process::id()));
        stage(&item.source, &staged).map_err(|e| failed(e, &installed))?;
        if let Err(e) = fs::rename(&staged, &to) {
            let _ = fs::remove_file(&staged);
            return Err(failed(
                format!("move {} -> {}: {e}", staged.display(), to.display()),
                &installed,
            ));
        }
        placed(item);
        installed.push(to);
    }
    Ok(installed)
}

/// `e`, naming what was already placed, which stays new.
fn already(e: String, installed: &[PathBuf]) -> String {
    let names: Vec<String> = installed
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    format!("{e} (already replaced: {})", names.join(", "))
}

/// Build every crate with `build`, each `(directory, binary)`, stopping at
/// the first that fails: nothing is installed until all are built.
fn build_all<'a>(
    crates: &[(&'a str, &'a str)],
    build: &mut dyn FnMut(&str, &str) -> Result<PathBuf, String>,
) -> Result<Vec<(&'a str, PathBuf)>, String> {
    crates
        .iter()
        .map(|(dir, bin)| build(dir, bin).map(|path| (*bin, path)))
        .collect()
}

/// What `td-net launch FLAG` lists, one per line: the programs `td_net`
/// launches by name (`--names`) or the companions it gives them
/// (`--companions`).
fn listed_by(td_net: &Path, flag: &str) -> Result<Vec<String>, String> {
    let output = Command::new(td_net)
        .args(["launch", flag])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", td_net.display()))?;
    if !output.status.success() {
        return Err(format!("td-net launch {flag} failed ({})", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .filter(|name| !name.is_empty())
        .collect())
}

/// Whether `dir` is one of `PATH`'s directories.
fn on_path(dir: &Path, path: Option<&OsStr>) -> bool {
    path.is_some_and(|path| std::env::split_paths(path).any(|entry| entry == dir))
}

/// Whether `dir` is a crate of the checkout at `root`.
fn checkout_crate(root: &Path, dir: &str) -> Result<(), String> {
    if root.join(dir).join("Cargo.toml").is_file() {
        return Ok(());
    }
    Err(format!(
        "{} is not the td checkout ({dir}/Cargo.toml is not under it): run \
         ./install-apps from the repository root",
        root.display()
    ))
}

/// A companion td-net names is built as the checkout's crate of that
/// name, so it must be a bare `td-` name.
fn companion_name(name: &str) -> Result<&str, String> {
    let bare = name.starts_with("td-")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if bare {
        Ok(name)
    } else {
        Err(format!(
            "td-net names a companion that is not a td crate: {name:?}"
        ))
    }
}

fn install_apps(root: &Path) -> Result<PathBuf, String> {
    for dir in APPS.iter().chain(&["net"]) {
        checkout_crate(root, dir)?;
    }
    let local = local_dir(std::env::var_os("HOME").as_deref())?;
    writable(&local.join("bin"))?;
    writable(&local.join(LIB))?;
    let tools = crate::host_run::tools(root)?;
    let td_net = crate::host_run::build(root, &tools, "net", TD_NET)?;
    let launched = listed_by(&td_net, "--names")?;
    let companions = listed_by(&td_net, "--companions")?;
    for (at, companion) in companions.iter().enumerate() {
        let elsewhere = APPS.contains(&companion.as_str())
            || companion == TD_NET
            || launched.contains(companion)
            || companions
                .get(..at)
                .is_some_and(|before| before.contains(companion));
        if elsewhere {
            return Err(format!(
                "td-net names {companion} as a companion twice or as another program too"
            ));
        }
        checkout_crate(root, companion_name(companion)?)?;
    }
    let crates: Vec<(&str, &str)> = APPS
        .iter()
        .copied()
        .chain(companions.iter().map(String::as_str))
        .map(|app| (app, app))
        .collect();
    let mut built = build_all(&crates, &mut |dir, bin| {
        crate::host_run::build(root, &tools, dir, bin)
    })?;
    built.push((TD_NET, td_net));
    let items = plan(&built, &launched, &companions, &local)?;
    install(&items, &mut |item| {
        let to = item.dir.join(&item.name);
        match &item.source {
            Source::File(_) => eprintln!("install-apps: installed {}", to.display()),
            Source::Link(target) => eprintln!(
                "install-apps: linked {} -> {} (td-net launch)",
                to.display(),
                target.display()
            ),
        }
    })?;
    Ok(local.join("bin"))
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    if !args.is_empty() {
        eprintln!("usage: td-builder install-apps");
        return ExitCode::from(2);
    }
    let root = match std::env::current_dir() {
        Ok(root) => root,
        Err(e) => {
            eprintln!("td-builder: install-apps: getcwd: {e}");
            return ExitCode::FAILURE;
        }
    };
    match install_apps(&root) {
        Ok(bin) => {
            if !on_path(&bin, std::env::var_os("PATH").as_deref()) {
                eprintln!(
                    "install-apps: {} is not on PATH; add it to run the programs by name",
                    bin.display()
                );
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("td-builder: install-apps: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("td-install-apps-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn file(dir: &Path, name: &str, from: &Path) -> Item {
        Item {
            dir: dir.to_path_buf(),
            name: name.into(),
            source: Source::File(from.to_path_buf()),
        }
    }

    #[test]
    fn the_programs_go_under_the_users_local_directory() {
        assert_eq!(
            local_dir(Some(OsStr::new("/h"))).unwrap(),
            Path::new("/h/.local")
        );
        assert!(local_dir(Some(OsStr::new("rel"))).is_err());
        assert!(local_dir(Some(OsStr::new(""))).is_err());
        assert!(local_dir(None).is_err());
    }

    #[test]
    fn a_companion_is_a_bare_td_crate_name() {
        assert_eq!(companion_name("td-jail"), Ok("td-jail"));
        assert_eq!(companion_name("td-txt2"), Ok("td-txt2"));
        for bad in [
            "",
            "jail",
            "td-../net",
            "td-jail/x",
            "../td-jail",
            "td-Jail",
            "td jail",
        ] {
            assert!(companion_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_launched_programs_live_with_td_net_and_their_names_link_to_it() {
        let local = Path::new("/h/.local");
        let built: Vec<(&str, PathBuf)> = ["td-editor", "td-jail", "td-mail", "td-net", "td-news"]
            .iter()
            .map(|name| (*name, PathBuf::from(format!("/t/{name}"))))
            .collect();
        let launched = vec!["td-mail".to_string(), "td-news".to_string()];
        let companions = vec!["td-jail".to_string()];
        let items = plan(&built, &launched, &companions, local).unwrap();
        let shown: Vec<(String, String, String)> = items
            .iter()
            .map(|item| {
                let source = match &item.source {
                    Source::File(from) => from.display().to_string(),
                    Source::Link(to) => format!("-> {}", to.display()),
                };
                (item.dir.display().to_string(), item.name.clone(), source)
            })
            .collect();
        let row = |dir: &str, name: &str, source: &str| {
            (dir.to_string(), name.to_string(), source.to_string())
        };
        assert_eq!(
            shown,
            [
                row("/h/.local/lib/td", "td-jail", "/t/td-jail"),
                row("/h/.local/lib/td", "td-mail", "/t/td-mail"),
                row("/h/.local/lib/td", "td-net", "/t/td-net"),
                row("/h/.local/lib/td", "td-news", "/t/td-news"),
                row("/h/.local/bin", "td-editor", "/t/td-editor"),
                row("/h/.local/bin", "td-mail", "-> /h/.local/lib/td/td-net"),
                row("/h/.local/bin", "td-news", "-> /h/.local/lib/td/td-net"),
            ]
        );
        // td-net launching a program not built here, or a launched program
        // without td-net, is refused.
        let more = vec!["td-chat".to_string()];
        assert!(plan(&built, &more, &companions, local).is_err());
        // Nor a companion td-net names that was not built.
        let txt = vec!["td-jail".to_string(), "td-txt".to_string()];
        assert!(plan(&built, &launched, &txt, local).is_err());
        let without: Vec<(&str, PathBuf)> = built
            .iter()
            .filter(|(name, _)| *name != TD_NET)
            .map(|(name, path)| (*name, path.clone()))
            .collect();
        assert!(plan(&without, &launched, &companions, local).is_err());
    }

    #[test]
    fn each_name_is_replaced_whole_a_running_program_keeping_its_file() {
        let base = scratch("install");
        let built = base.join("built");
        fs::create_dir_all(&built).unwrap();
        for (name, bytes) in [("td-a", &b"new a"[..]), ("td-net", b"net")] {
            fs::write(built.join(name), bytes).unwrap();
            fs::set_permissions(built.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let (bin, lib) = (base.join("local/bin"), base.join("local/lib/td"));
        let items = [
            file(&lib, "td-net", &built.join("td-net")),
            file(&bin, "td-a", &built.join("td-a")),
            Item {
                dir: bin.clone(),
                name: "td-b".into(),
                source: Source::Link(lib.join("td-net")),
            },
        ];
        install(&items, &mut |_| {}).unwrap();
        fs::write(built.join("td-a"), b"newer a").unwrap();
        let old = fs::File::open(bin.join("td-a")).unwrap();
        let mut told = Vec::new();
        let installed = install(&items, &mut |item| told.push(item.name.clone())).unwrap();
        assert_eq!(told, ["td-net", "td-a", "td-b"], "told in order as placed");
        assert_eq!(
            installed,
            [lib.join("td-net"), bin.join("td-a"), bin.join("td-b")]
        );
        assert_eq!(fs::read(bin.join("td-a")).unwrap(), b"newer a");
        let mut kept = Vec::new();
        std::io::Read::read_to_end(&mut &old, &mut kept).unwrap();
        assert_eq!(kept, b"new a");
        assert_eq!(fs::read_link(bin.join("td-b")).unwrap(), lib.join("td-net"));
        assert_eq!(
            fs::read(bin.join("td-b")).unwrap(),
            b"net",
            "the link resolves"
        );
        for path in [lib.join("td-net"), bin.join("td-a")] {
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o7777, 0o755, "{}", path.display());
        }
        assert_eq!(names(&bin), ["td-a", "td-b"], "nothing staged is left");
        assert_eq!(names(&lib), ["td-net"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_failure_names_what_was_already_placed_and_leaves_nothing_staged() {
        let base = scratch("missing");
        let dest = base.join("bin");
        let why = install(&[file(&dest, "td-a", &base.join("absent"))], &mut |_| {}).unwrap_err();
        assert!(why.contains("absent"), "{why}");
        assert!(!why.contains("already replaced"), "{why}");
        assert!(names(&dest).is_empty());
        fs::write(base.join("built-a"), b"a").unwrap();
        let items = [
            file(&dest, "td-a", &base.join("built-a")),
            file(&dest, "td-b", &base.join("absent")),
        ];
        let why = install(&items, &mut |_| {}).unwrap_err();
        assert!(
            why.contains(&format!(
                "already replaced: {}",
                dest.join("td-a").display()
            )),
            "{why}"
        );
        assert_eq!(names(&dest), ["td-a"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn the_first_failed_build_stops_the_builds() {
        let mut asked = Vec::new();
        let mut build = |dir: &str, bin: &str| {
            asked.push(format!("{dir}/{bin}"));
            if bin == "td-b" {
                Err("td-b failed".to_string())
            } else {
                Ok(PathBuf::from(format!("/t/{bin}")))
            }
        };
        let why =
            build_all(&[("a", "td-a"), ("b", "td-b"), ("c", "td-c")], &mut build).unwrap_err();
        assert_eq!(why, "td-b failed");
        assert_eq!(asked, ["a/td-a", "b/td-b"]);
        let built = build_all(&[("a", "td-a"), ("net", "td-net")], &mut |_, bin| {
            Ok(PathBuf::from(format!("/t/{bin}")))
        })
        .unwrap();
        assert_eq!(
            built,
            [
                ("td-a", PathBuf::from("/t/td-a")),
                ("td-net", PathBuf::from("/t/td-net"))
            ]
        );
    }

    #[test]
    fn the_links_resolve_when_local_bin_is_itself_a_link() {
        let base = scratch("linked-bin");
        let (elsewhere, local) = (base.join("dotfiles/bin"), base.join("home/.local"));
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(&local).unwrap();
        std::os::unix::fs::symlink(&elsewhere, local.join("bin")).unwrap();
        fs::write(base.join("td-net"), b"net").unwrap();
        fs::write(base.join("td-news"), b"news").unwrap();
        let built = [
            ("td-net", base.join("td-net")),
            ("td-news", base.join("td-news")),
        ];
        let items = plan(&built, &["td-news".to_string()], &[], &local).unwrap();
        install(&items, &mut |_| {}).unwrap();
        assert_eq!(fs::read(local.join("bin/td-news")).unwrap(), b"net");
        assert_eq!(fs::read(elsewhere.join("td-news")).unwrap(), b"net");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_name_that_changes_kind_between_installs_is_replaced() {
        let base = scratch("kind");
        let dest = base.join("bin");
        fs::create_dir_all(&dest).unwrap();
        fs::write(base.join("built"), b"new").unwrap();
        // A program where a link now goes, and a link where a program now
        // goes.
        fs::write(dest.join("td-link"), b"old program").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), dest.join("td-file")).unwrap();
        let items = [
            Item {
                dir: dest.clone(),
                name: "td-link".into(),
                source: Source::Link(base.join("built")),
            },
            file(&dest, "td-file", &base.join("built")),
        ];
        install(&items, &mut |_| {}).unwrap();
        assert_eq!(
            fs::read_link(dest.join("td-link")).unwrap(),
            base.join("built")
        );
        assert!(fs::symlink_metadata(dest.join("td-file"))
            .unwrap()
            .is_file());
        assert_eq!(fs::read(dest.join("td-file")).unwrap(), b"new");
        assert!(!base.join("elsewhere").exists());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_killed_installs_staged_link_is_swept_and_its_target_kept() {
        let base = scratch("sweep-link");
        let dest = base.join("bin");
        let target = base.join("kept");
        fs::create_dir_all(&dest).unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("inside"), b"kept").unwrap();
        std::os::unix::fs::symlink(&target, dest.join(".td-a.install-4294967295")).unwrap();
        fs::write(base.join("built"), b"a").unwrap();
        install(&[file(&dest, "td-a", &base.join("built"))], &mut |_| {}).unwrap();
        assert_eq!(names(&dest), ["td-a"]);
        assert_eq!(fs::read(target.join("inside")).unwrap(), b"kept");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_killed_installs_staged_name_is_swept_by_the_next() {
        let base = scratch("sweep");
        let dest = base.join("bin");
        fs::create_dir_all(&dest).unwrap();
        // No process has pid 4294967295.
        fs::write(dest.join(".td-a.install-4294967295"), b"partial").unwrap();
        fs::write(base.join("built"), b"a").unwrap();
        install(&[file(&dest, "td-a", &base.join("built"))], &mut |_| {}).unwrap();
        assert_eq!(names(&dest), ["td-a"]);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_staged_name_another_install_holds_is_refused_not_truncated() {
        let base = scratch("collide");
        let dest = base.join("bin");
        fs::create_dir_all(&dest).unwrap();
        // Another live install, with this pid in its own namespace, staging
        // td-a: its file is left whole, nothing is installed.
        let theirs = dest.join(format!(".td-a.install-{}", std::process::id()));
        fs::write(&theirs, b"theirs").unwrap();
        fs::write(base.join("built"), b"mine").unwrap();
        let why = install(&[file(&dest, "td-a", &base.join("built"))], &mut |_| {}).unwrap_err();
        assert!(why.contains("stage"), "{why}");
        assert_eq!(fs::read(&theirs).unwrap(), b"theirs");
        assert!(!dest.join("td-a").exists());
        assert!(why.contains("a rerun clears it"), "{why}");
        // So is a link staged over it.
        let link = Item {
            dir: dest.clone(),
            name: "td-a".into(),
            source: Source::Link(base.join("built")),
        };
        assert!(install(&[link], &mut |_| {}).unwrap_err().contains("stage"));
        assert_eq!(fs::read(&theirs).unwrap(), b"theirs");
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn an_unwritable_directory_is_refused_before_anything_is_built() {
        let base = scratch("writable");
        let dest = base.join("home/.local/bin");
        // A probe a killed install left is swept first.
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join(".install-apps.probe-4294967295"), b"").unwrap();
        writable(&dest).unwrap();
        assert!(names(&dest).is_empty(), "the probe is removed");
        // A link at the probe's name is refused, never followed.
        let precious = base.join("precious");
        fs::write(&precious, b"config").unwrap();
        let probe = dest.join(format!(".install-apps.probe-{}", std::process::id()));
        std::os::unix::fs::symlink(&precious, &probe).unwrap();
        assert!(writable(&dest).is_err());
        assert_eq!(fs::read(&precious).unwrap(), b"config");
        fs::remove_file(&probe).unwrap();
        let blocked = base.join("blocked");
        fs::write(&blocked, b"").unwrap();
        assert!(writable(&blocked.join("bin")).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn local_bin_is_on_path_only_as_a_whole_entry() {
        let dir = Path::new("/h/.local/bin");
        assert!(on_path(dir, Some(OsStr::new("/usr/bin:/h/.local/bin"))));
        assert!(!on_path(dir, Some(OsStr::new("/usr/bin:/h/.local/bin/x"))));
        assert!(!on_path(dir, None));
    }
}
