//! `td-btrfs`, and `mkfs.btrfs` when invoked by that name.

// The recipe compiles with rustc directly, which reads no Cargo lints.
#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use td_btrfs::check::check;
use td_btrfs::export::Contents;
use td_btrfs::format::{invalid, parse_uuid};
use td_btrfs::mkfs::{self, parse_size, Options};
use td_btrfs::read::Volume;

const USAGE: &str =
    "usage: mkfs.btrfs [-q] [-b|--byte-count SIZE] [-U|--uuid UUID] [-L|--label LABEL]
                  [-r|--rootdir DIR [--subvol [rw:]PATH]...] TARGET
       td-btrfs mkfs ARGS...
       td-btrfs check IMAGE
       td-btrfs list IMAGE
       td-btrfs restore IMAGE EMPTY-DIR
";

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let name = Path::new(&argv0)
        .file_name()
        .map(|name| name.as_bytes().to_vec())
        .unwrap_or_default();
    let rest: Vec<OsString> = args.collect();
    let result = if name == b"mkfs.btrfs" {
        run_mkfs(&rest)
    } else {
        match rest.split_first() {
            Some((command, rest)) if command == "mkfs" => run_mkfs(rest),
            Some((command, [image])) if command == "check" => run_check(Path::new(image)),
            Some((command, [image])) if command == "list" => Volume::open(Path::new(image))
                .and_then(Contents::read)
                .and_then(|contents| contents.list(&mut io::stdout().lock())),
            Some((command, [image, dest])) if command == "restore" => {
                Volume::open(Path::new(image))
                    .and_then(Contents::read)
                    .and_then(|contents| contents.restore(Path::new(dest)))
            }
            _ => Err(invalid(USAGE)),
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr(), "ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}

fn text(value: &OsString) -> io::Result<&str> {
    value
        .to_str()
        .ok_or_else(|| invalid(format!("{value:?} is not UTF-8")))
}

/// Parse the btrfs-progs `mkfs.btrfs` arguments td uses.
fn parse_mkfs(args: &[OsString]) -> io::Result<(Options, bool)> {
    let mut options = Options::default();
    let mut quiet = false;
    let mut target = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .ok_or_else(|| invalid(format!("{flag} needs a value\n{USAGE}")))
        };
        match arg.to_str() {
            Some("-q" | "--quiet") => quiet = true,
            Some(flag @ ("-b" | "--byte-count")) => {
                options.byte_count = Some(parse_size(text(value(flag)?)?)?);
            }
            Some(flag @ ("-U" | "--uuid")) => {
                let uuid = parse_uuid(text(value(flag)?)?)?;
                if uuid == [0; 16] {
                    return Err(invalid("the filesystem UUID cannot be zero"));
                }
                options.uuid = Some(uuid);
            }
            Some(flag @ ("-L" | "--label")) => {
                options.label = value(flag)?.as_bytes().to_vec();
            }
            Some(flag @ ("-r" | "--rootdir")) => {
                options.rootdir = Some(PathBuf::from(value(flag)?));
            }
            Some(flag @ "--subvol") => {
                let spec = value(flag)?.as_bytes();
                let path = match spec.iter().position(|byte| *byte == b':') {
                    Some(at) => match spec.split_at(at) {
                        (b"rw", path) => path.get(1..).unwrap_or_default(),
                        (kind, _) => {
                            return Err(invalid(format!(
                                "--subvol type {:?} is not supported; only rw is",
                                String::from_utf8_lossy(kind)
                            )))
                        }
                    },
                    None => spec,
                };
                if path.is_empty() {
                    return Err(invalid("--subvol needs a path"));
                }
                options
                    .subvols
                    .push(PathBuf::from(std::ffi::OsStr::from_bytes(path)));
            }
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                return Err(invalid(format!("unknown option {flag}\n{USAGE}")));
            }
            _ => {
                if target.replace(PathBuf::from(arg)).is_some() {
                    return Err(invalid(format!("only one target is supported\n{USAGE}")));
                }
            }
        }
    }
    options.target = target.ok_or_else(|| invalid(format!("no target\n{USAGE}")))?;
    Ok((options, quiet))
}

fn run_mkfs(args: &[OsString]) -> io::Result<()> {
    let (options, quiet) = parse_mkfs(args)?;
    let made = mkfs::make(&options)?;
    if !quiet {
        mkfs::report(&made, &options.label, &mut io::stdout().lock())?;
    }
    Ok(())
}

fn run_check(image: &Path) -> io::Result<()> {
    let report = check(image)?;
    let mut out = io::stdout().lock();
    for error in &report.errors {
        writeln!(out, "error: {error}")?;
    }
    writeln!(
        out,
        "{}: {} tree blocks, {} data bytes, {} checksum bytes, {} inodes in {} subvolumes",
        image.display(),
        report.tree_blocks,
        report.data_bytes,
        report.csum_bytes,
        report.inodes,
        report.subvolumes
    )?;
    if report.errors.is_empty() {
        writeln!(out, "no errors found")?;
        Ok(())
    } else {
        Err(invalid(format!("{} errors found", report.errors.len())))
    }
}
