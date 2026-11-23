//! Build td's PostgreSQL closure from the controller's clean checkout and
//! dump each store path as a NAR the production controller verifies.
use super::Local;
use crate::{
    common::{child, digest, fmt, owned, require, store_basename},
    R,
};
use std::{
    fs::{self, File},
    io::{self, Read},
    os::fd::AsFd,
    path::{Component, Path, PathBuf},
    process::Command,
    time::Duration,
};

/// The output stems, PostgreSQL first; td glibc is its runtime closure.
pub(super) const OUTPUTS: &[&str] = &["postgresql-x86-64", "glibc-x86-64"];
const BUILD_LIMIT: Duration = Duration::from_secs(6 * 3600);
// The PostgreSQL output with its debug companions is hundreds of megabytes.
const DUMP_LIMIT: Duration = Duration::from_secs(30 * 60);

pub(super) struct Built {
    pub(super) basename: String,
    pub(super) nar: PathBuf,
    pub(super) sha256: String,
}

/// Each OUTPUTS stem's staged path from `build-run`'s report, in order.
pub(super) fn run_outputs(report: &str) -> R<Vec<PathBuf>> {
    let mut found: Vec<Option<&str>> = td_alloc::filled(OUTPUTS.len(), None)?;
    for line in report.lines() {
        let Some(rest) = line.strip_prefix("TD_RECIPE_RUN_OUT ") else {
            continue;
        };
        let (stem, path) = rest.split_once(' ').ok_or("unexpected build-run output")?;
        let slot = OUTPUTS
            .iter()
            .position(|output| *output == stem)
            .and_then(|index| found.get_mut(index))
            .ok_or("unexpected build-run output")?;
        require(slot.is_none(), "repeated build-run output")?;
        *slot = Some(path);
    }
    let mut paths = Vec::new();
    for (stem, path) in OUTPUTS.iter().zip(found) {
        let path = Path::new(path.ok_or("missing build-run output")?);
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("unexpected build-run output")?;
        require(path.is_absolute(), "unexpected build-run output")?;
        store_basename(name, stem)?;
        td_alloc::push(
            &mut paths,
            PathBuf::from(owned(path.to_str().ok_or("build-run path")?)?),
        )?;
    }
    Ok(paths)
}

/// What a `readelf -h -l -d -W` report says of one ELF file.
#[derive(Default)]
pub(super) struct Elf<'a> {
    /// An executable or shared object, which the loader maps.
    pub(super) loadable: bool,
    pub(super) interpreter: Option<&'a str>,
    pub(super) soname: Option<&'a str>,
    pub(super) needed: Vec<&'a str>,
    runpath: Vec<&'a str>,
    rpath: Vec<&'a str>,
}

impl<'a> Elf<'a> {
    /// Where the loader looks for this object's own NEEDED entries: its
    /// DT_RUNPATH, or its DT_RPATH when it has no DT_RUNPATH.
    pub(super) fn run_paths(&self) -> &[&'a str] {
        if self.runpath.is_empty() {
            &self.rpath
        } else {
            &self.runpath
        }
    }

    /// The interpreter and every run-path entry.
    pub(super) fn references(&self) -> impl Iterator<Item = &&'a str> {
        self.interpreter
            .iter()
            .chain(&self.runpath)
            .chain(&self.rpath)
    }
}

fn bracketed<'a>(line: &'a str, label: &str) -> R<Option<&'a str>> {
    match line.split_once(label) {
        None => Ok(None),
        Some((_, rest)) => Ok(Some(
            rest.strip_suffix(']').ok_or("unexpected readelf output")?,
        )),
    }
}

/// One file's `readelf -h -l -d -W` report.
pub(super) fn parse(report: &str) -> R<Elf<'_>> {
    let mut elf = Elf::default();
    for line in report.lines() {
        let line = line.trim();
        if let Some(kind) = line.strip_prefix("Type:") {
            let kind = kind.trim_start();
            elf.loadable = kind.starts_with("EXEC ") || kind.starts_with("DYN ");
        } else if let Some(interpreter) = bracketed(line, "[Requesting program interpreter: ")? {
            elf.interpreter = Some(interpreter);
        } else if let Some(list) = bracketed(line, "Library runpath: [")? {
            for entry in list.split(':') {
                td_alloc::push(&mut elf.runpath, entry)?;
            }
        } else if let Some(list) = bracketed(line, "Library rpath: [")? {
            for entry in list.split(':') {
                td_alloc::push(&mut elf.rpath, entry)?;
            }
        } else if let Some(library) = bracketed(line, "Shared library: [")? {
            require(
                !library.contains('/'),
                "an ELF file loads a library by path",
            )?;
            td_alloc::push(&mut elf.needed, library)?;
        } else if let Some(soname) = bracketed(line, "Library soname: [")? {
            elf.soname = Some(soname);
        }
    }
    Ok(elf)
}

// `entry` of `object`'s run path with $ORIGIN replaced by its directory.
fn expand(entry: &str, object: &str) -> R<String> {
    match entry
        .strip_prefix("$ORIGIN")
        .or_else(|| entry.strip_prefix("${ORIGIN}"))
    {
        Some(rest) => {
            let (origin, _) = object.rsplit_once('/').ok_or("ELF object path")?;
            fmt(format_args!("{origin}{rest}"))
        }
        None => owned(entry),
    }
}

/// Load `root` as td's loader does, breadth first, and refuse unless every
/// NEEDED name resolves inside the closure: to an object already loaded
/// under that name or soname, or else through the needing object's own run
/// paths, never the loader's default directories or cache. `seed` is what
/// the process already loaded (the loader); `find` maps a candidate path to
/// the object of `objects` it opens, if the file exists.
pub(super) fn resolve<'a>(
    root: usize,
    objects: &'a [(&'a str, Elf<'a>)],
    seed: &[&'a str],
    find: &dyn Fn(&str) -> R<Option<String>>,
) -> R<()> {
    let (root_path, root_elf) = objects.get(root).ok_or("ELF root")?;
    let mut loaded = td_alloc::copy(seed)?;
    td_alloc::extend_from_slice(&mut loaded, root_elf.soname.as_slice())?;
    let mut walk = td_alloc::with_capacity(objects.len())?;
    td_alloc::push(&mut walk, root)?;
    let mut next = 0;
    while let Some(&index) = walk.get(next) {
        next += 1;
        let (path, elf) = objects.get(index).ok_or("ELF index")?;
        for &name in &elf.needed {
            if loaded.contains(&name) {
                continue;
            }
            let mut found = None;
            for entry in elf.run_paths() {
                let candidate = fmt(format_args!("{}/{name}", expand(entry, path)?))?;
                if let Some(target) = find(&candidate)? {
                    let Some(position) = objects.iter().position(|(object, _)| *object == target)
                    else {
                        return Err(fmt(format_args!("{candidate} is not an ELF object"))?.into());
                    };
                    found = Some(position);
                    break;
                }
            }
            let Some(found) = found else {
                return Err(fmt(format_args!(
                    "{root_path}: {path} needs {name}, which nothing loaded and none of its own run paths provide"
                ))?
                .into());
            };
            td_alloc::push(&mut loaded, name)?;
            if let Some((_, target)) = objects.get(found) {
                td_alloc::extend_from_slice(&mut loaded, target.soname.as_slice())?;
            }
            if !walk.contains(&found) {
                td_alloc::push(&mut walk, found)?;
            }
        }
    }
    Ok(())
}

/// Resolve every executable and shared object of the PostgreSQL output
/// (`/td/store/POSTGRES/...`) as its own root: what the host runs and what
/// its server loads. A shared object is seeded with the closure's loader,
/// which every process that loads it already has. td glibc's own objects
/// are resolved where these roots load them.
pub(super) fn check_loading(
    objects: &[(&str, Elf<'_>)],
    postgres: &str,
    find: &dyn Fn(&str) -> R<Option<String>>,
) -> R<()> {
    let mut loaders = Vec::new();
    for (_, elf) in objects {
        if let Some(name) = elf.interpreter.and_then(|path| path.rsplit('/').next()) {
            if !loaders.contains(&name) {
                td_alloc::push(&mut loaders, name)?;
            }
        }
    }
    let prefix = fmt(format_args!("/td/store/{postgres}/"))?;
    let mut roots = 0;
    for (index, (path, elf)) in objects.iter().enumerate() {
        if elf.loadable && path.starts_with(prefix.as_str()) {
            resolve(index, objects, &loaders, find)?;
            roots += 1;
        }
    }
    require(roots > 0, "the PostgreSQL output loads nothing")
}

/// Whether a reference stays inside one of `basenames`'s store paths:
/// absolute under it, or relative to the referring file's own directory.
pub(super) fn in_closure(reference: &str, basenames: &[&str]) -> bool {
    let (path, absolute) = match reference
        .strip_prefix("$ORIGIN")
        .or_else(|| reference.strip_prefix("${ORIGIN}"))
    {
        Some(rest) => (rest, false),
        None => (reference, true),
    };
    let normal = Path::new(path)
        .components()
        .all(|c| matches!(c, Component::RootDir | Component::Normal(_)));
    if !absolute {
        return normal && (path.is_empty() || path.starts_with('/'));
    }
    normal
        && path.strip_prefix("/td/store/").is_some_and(|rest| {
            basenames.contains(&rest.split_once('/').map_or(rest, |(name, _)| name))
        })
}

// The closure object a run-path candidate `/td/store/NAME/...` opens in the
// staged outputs, following symbolic links; None if no file is there.
fn staged_object(
    candidate: &str,
    outputs: &[PathBuf],
    names: &[&str],
    real: &[(PathBuf, &str)],
) -> R<Option<String>> {
    let Some(rest) = candidate.strip_prefix("/td/store/") else {
        return Ok(None);
    };
    let (name, tail) = rest.split_once('/').unwrap_or((rest, ""));
    let Some(root) = names
        .iter()
        .position(|n| *n == name)
        .and_then(|index| outputs.get(index))
    else {
        return Ok(None);
    };
    let target = match fs::canonicalize(child(root, tail)?) {
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(e) => return Err(e.into()),
        Ok(target) => target,
    };
    for (root, name) in real {
        if let Ok(relative) = target.strip_prefix(root) {
            let relative = relative.to_str().ok_or("ELF file name")?;
            return Ok(Some(fmt(format_args!("/td/store/{name}/{relative}"))?));
        }
    }
    Err(fmt(format_args!(
        "{candidate} resolves outside the PostgreSQL closure"
    ))?
    .into())
}

fn is_elf(path: &Path) -> R<bool> {
    let mut magic = [0u8; 4];
    match File::open(path)?.read_exact(&mut magic) {
        Ok(()) => Ok(magic == *b"\x7fELF"),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

// Every ELF file under `dir` except the debug companions, which no loader
// reads; the runtime file each pairs with is checked.
fn elf_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> R<()> {
    let companions = child(root, "lib/debug")?;
    for item in fs::read_dir(dir)? {
        let item = item?;
        let kind = item.file_type()?;
        let path = item.path();
        if kind.is_dir() && path != companions {
            elf_files(root, &path, out)?;
        } else if kind.is_file() && is_elf(&path)? {
            td_alloc::push(out, path)?;
        }
    }
    Ok(())
}

impl Local {
    /// Build both outputs in `temp/source`, require that every interpreter
    /// and run path stays inside them and that everything the PostgreSQL
    /// output loads resolves there, and dump each as a NAR in `temp`.
    pub(super) fn closure(&self, temp: &Path) -> R<Vec<Built>> {
        let checkout = child(temp, "source")?;
        for args in [
            &["--manifest-path", "builder/Cargo.toml"][..],
            &[
                "--manifest-path",
                "recipes/Cargo.toml",
                "--bin",
                "td-recipe-eval",
            ][..],
        ] {
            let mut build = self.cargo(&checkout)?;
            build.args(["build", "--release", "--locked"]).args(args);
            require(
                build.status()?.success(),
                "building td's recipe tooling failed",
            )?;
        }
        let release = child(&checkout, "target/release")?;
        let builder = child(&release, "td-builder")?;
        let report = child(temp, "build-run.out")?;
        let stderr = File::from(io::stderr().as_fd().try_clone_to_owned()?);
        self.host.run_to(
            Command::new(child(&release, "td-recipe-eval")?)
                .current_dir(&checkout)
                .env("TD_BUILDER_SELF", &builder)
                .args(["build-run", "postgresql-x86-64"])
                .args(OUTPUTS),
            BUILD_LIMIT,
            &File::create_new(&report)?,
            Some(&stderr),
        )?;
        let outputs = run_outputs(std::str::from_utf8(&td_fs::read_bounded(
            &report,
            1 << 24,
        )?)?)?;
        let mut names = Vec::new();
        for path in &outputs {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("output name")?;
            td_alloc::push(&mut names, name)?;
        }
        let postgres = outputs.first().ok_or("missing PostgreSQL output")?;
        require(
            is_elf(&child(postgres, "bin/postgres")?)?,
            "the PostgreSQL output has no server",
        )?;
        let mut reports = Vec::new();
        for (root, name) in outputs.iter().zip(&names) {
            let mut files = Vec::new();
            elf_files(root, root, &mut files)?;
            for file in files {
                let relative = file.strip_prefix(root)?.to_str().ok_or("ELF file name")?;
                let report = self.host.out(
                    Command::new("readelf")
                        .env("LC_ALL", "C")
                        .args(["-h", "-l", "-d", "-W"])
                        .arg(&file),
                )?;
                td_alloc::push(
                    &mut reports,
                    (fmt(format_args!("/td/store/{name}/{relative}"))?, report),
                )?;
            }
        }
        let mut objects = Vec::new();
        for (path, report) in &reports {
            let elf = parse(report)?;
            for reference in elf.references() {
                if !in_closure(reference, &names) {
                    return Err(fmt(format_args!(
                        "{path} loads {reference}, outside the PostgreSQL closure"
                    ))?
                    .into());
                }
            }
            td_alloc::push(&mut objects, (path.as_str(), elf))?;
        }
        let mut real = Vec::new();
        for (root, name) in outputs.iter().zip(&names) {
            td_alloc::push(&mut real, (fs::canonicalize(root)?, *name))?;
        }
        let find = |candidate: &str| staged_object(candidate, &outputs, &names, &real);
        check_loading(&objects, names.first().ok_or("PostgreSQL output")?, &find)?;
        let mut built = Vec::new();
        for (path, name) in outputs.iter().zip(names) {
            let nar = child(temp, &fmt(format_args!("{name}.nar"))?)?;
            let printed = child(temp, &fmt(format_args!("{name}.nar-dump"))?)?;
            self.host.run_to(
                Command::new(&builder).arg("nar-dump").arg(path).arg(&nar),
                DUMP_LIMIT,
                &File::create_new(&printed)?,
                Some(&stderr),
            )?;
            let line = td_fs::read_bounded(&printed, 4096)?;
            let sha256 = std::str::from_utf8(&line)?
                .trim()
                .strip_prefix("sha256:")
                .filter(|hex| {
                    hex.len() == 64
                        && hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                })
                .ok_or("unexpected nar-dump output")?;
            require(digest(&nar)? == sha256, "NAR digest")?;
            println!("PostgreSQL closure: {name} sha256:{sha256}");
            td_alloc::push(
                &mut built,
                Built {
                    basename: owned(name)?,
                    nar,
                    sha256: owned(sha256)?,
                },
            )?;
        }
        Ok(built)
    }
}
