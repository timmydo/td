//! `tar`: extraction over the engine's bounded archive reader, and creation
//! with the engine's ustar writer, which a td-built `tar -x` and GNU tar read.
use crate::tar_write::{self, Header, DIRECTORY, REGULAR, SYMLINK};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, BufReader, BufWriter, Cursor, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

const MAX_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GZIP_BYTES: usize = 1024 * 1024 * 1024;
/// What a file is copied through, allocated once per archive.
const COPY_BUFFER: usize = 64 * 1024;
/// open(2)'s flags on x86-64 Linux, raw for `custom_flags`: a file replaced by
/// a symlink after it was looked at is not followed, nor a FIFO waited on, nor a
/// terminal made this process's own.
const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_NOCTTY: i32 = 0o400;
const HELP: &str = "Usage: tar -x [-z] -f ARCHIVE [-C DIRECTORY]\n\
       tar -c [-z] -f ARCHIVE [-C DIRECTORY] [--] PATH...\n\
Extract or create a tar or gzip-compressed tar archive. Combined flags (-xzf, -czf)\n\
are supported; unsupported options are errors.\n\
\n\
Extraction:\n\
Limits: 1 GiB per file, 16 GiB total payload, 1 million entries.\n\
Gzip limits: 257 MiB compressed, 1 GiB expanded (decoded in memory).\n\
Modes honor the process umask; special permission bits are stripped.\n\
Directory modes and mtimes are restored after extraction.\n\
Paths may not escape the destination or traverse symlink ancestors.\n\
Extraction may leave partial output on error; use a private destination.\n\
\n\
Creation:\n\
-f - writes to standard output. Each PATH is relative to -C's directory and may not\n\
be absolute or hold `..`; `.` stores that directory's contents.\n\
Directories are walked in byte order. Files, directories and symlinks are stored with\n\
their permission bits, numeric owners and mtimes, one before 1970 as 1970; a symlink is\n\
not followed, and a hard link is stored as another copy. Sockets are left out with a\n\
note; other special files are errors, as are names and targets that are not UTF-8,\n\
which extraction refuses. Names and targets past 100 bytes take GNU long-name records.\n\
-C applies to every PATH and comes before them, and a PATH may not pass through a\n\
symlink, which extraction refuses. A file that changes while it is stored is noted;\n\
one that cannot be read ends the archive, as does a path past the kernel's limit.\n\
-z builds the archive in memory, refused past what extraction reads: 1 GiB expanded,\n\
257 MiB compressed.\n\
\n\
Exit status: 0, done; 1, an archive made whole but with a file that changed while it\n\
was stored; 2, an error, after which an archive being made is incomplete.\n";

#[derive(Clone, Copy)]
enum Mode {
    Extract,
    Create,
}

/// Every error is status 2, as GNU tar's is, so that `-c`'s 1 says only that a
/// file changed in an archive that is whole.
pub fn run(args: &[String]) -> Result<u8, String> {
    match tar(args) {
        Ok(status) => Ok(status),
        Err(error) => {
            crate::emit_err(&format!("tar: {error}\n"));
            Ok(2)
        }
    }
}

fn tar(args: &[String]) -> Result<u8, String> {
    if args.len() == 1 && args.first().is_some_and(|s| s == "--help") {
        crate::emit(HELP)?;
        return Ok(0);
    }
    let mut mode = None;
    let mut gzip = false;
    let mut archive = None;
    let mut directory = None;
    let mut paths = Vec::new();
    let mut options = true;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if options && arg == "--" {
            options = false;
            continue;
        }
        let Some(flags) = arg
            .strip_prefix('-')
            .filter(|flags| options && !flags.is_empty())
        else {
            paths.push(arg.as_str());
            continue;
        };
        for (offset, flag) in flags.char_indices() {
            match flag {
                'x' if mode.is_none() => mode = Some(Mode::Extract),
                'c' if mode.is_none() => mode = Some(Mode::Create),
                'z' if !gzip => gzip = true,
                'f' | 'C' => {
                    let rest = arg.get(offset + 2..).unwrap_or("");
                    let value = if rest.is_empty() {
                        args.next()
                            .map(String::as_str)
                            .ok_or_else(|| format!("-{flag} requires a value"))?
                    } else {
                        rest
                    };
                    if flag == 'C' && !paths.is_empty() {
                        return Err("-C comes before the PATHs it applies to".to_string());
                    }
                    let slot = if flag == 'f' {
                        &mut archive
                    } else {
                        &mut directory
                    };
                    if slot.replace(value).is_some() {
                        return Err(format!("duplicate -{flag}"));
                    }
                    break;
                }
                _ => {
                    return Err(format!(
                        "unsupported or duplicate option -{flag}; see tar --help"
                    ))
                }
            }
        }
    }
    match mode {
        Some(Mode::Extract) => {
            if let Some(path) = paths.first() {
                return Err(format!("unsupported operand {path}; see tar --help"));
            }
            extract(gzip, archive, directory)
        }
        Some(Mode::Create) => create(gzip, archive, directory, &paths),
        None => Err("-x or -c is required; see tar --help".to_string()),
    }
}

fn extract(gzip: bool, archive: Option<&str>, directory: Option<&str>) -> Result<u8, String> {
    let archive = archive
        .filter(|s| !s.is_empty() && *s != "-")
        .ok_or_else(|| "-f requires an archive filename (stdin is unsupported)".to_string())?;
    let dest = Path::new(directory.unwrap_or("."));
    if !dest.is_dir() {
        return Err(format!(
            "destination is not a directory: {}",
            dest.display()
        ));
    }
    let mask = process_umask()?;
    if gzip {
        let bytes = crate::gzip::decompress_file_with_limit(Path::new(archive), MAX_GZIP_BYTES)?;
        crate::tar_extract::extract_tar_reader_with_options(
            &mut Cursor::new(bytes),
            archive,
            dest,
            MAX_ENTRY_BYTES,
            Some(mask),
        )?;
    } else {
        let file = File::open(archive).map_err(|e| format!("open {archive}: {e}"))?;
        crate::tar_extract::extract_tar_reader_with_options(
            &mut BufReader::new(file),
            archive,
            dest,
            MAX_ENTRY_BYTES,
            Some(mask),
        )?;
    }
    Ok(0)
}

fn create(
    gzip: bool,
    archive: Option<&str>,
    directory: Option<&str>,
    paths: &[&str],
) -> Result<u8, String> {
    let archive = archive
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "-f requires an archive filename, or - for standard output".to_string())?;
    if paths.is_empty() {
        return Err("refusing to create an empty archive: name a PATH; see tar --help".to_string());
    }
    let names = paths
        .iter()
        .map(|path| member_name(path))
        .collect::<Result<Vec<_>, _>>()?;
    let base = Path::new(directory.unwrap_or("."));
    if !base.is_dir() {
        return Err(format!("not a directory: {}", base.display()));
    }
    for name in &names {
        through_no_symlink(base, name)?;
    }
    let (output, archive_meta) = if archive == "-" {
        let stdout = io::stdout();
        // Standard output may be a file in the tree, as `tar -cf - . > a.tar` makes it.
        let meta = stdout
            .as_fd()
            .try_clone_to_owned()
            .ok()
            .and_then(|fd| File::from(fd).metadata().ok());
        (Box::new(stdout.lock()) as Box<dyn Write>, meta)
    } else {
        let file = File::create(archive).map_err(|e| format!("create {archive}: {e}"))?;
        let meta = file.metadata().ok();
        (Box::new(file) as Box<dyn Write>, meta)
    };
    let mut packer = Packer {
        archive: archive_meta
            .filter(Metadata::is_file)
            .map(|meta| (meta.dev(), meta.ino())),
        buffer: vec![0; COPY_BUFFER],
        changed: false,
    };
    let written = |e: io::Error| format!("write {archive}: {e}");
    if gzip {
        let mut tar = Bounded {
            bytes: Vec::new(),
            limit: MAX_GZIP_BYTES,
        };
        packer.pack(&mut tar, base, &names)?;
        let compressed = crate::deflate::gzip(&tar.bytes, 6);
        drop(tar);
        if compressed.len() as u64 > crate::gzip::MAX_GZIP_INPUT_BYTES {
            return Err(format!(
                "-z made {} bytes, past the 257 MiB tar -x -z reads; leave out -z",
                compressed.len()
            ));
        }
        let mut output = output;
        output
            .write_all(&compressed)
            .and_then(|()| output.flush())
            .map_err(written)?;
    } else {
        let mut output = BufWriter::with_capacity(COPY_BUFFER, output);
        packer.pack(&mut output, base, &names)?;
        output.flush().map_err(written)?;
    }
    Ok(u8::from(packer.changed))
}

/// The member name an operand stands for: its parts joined by `/`, with `.`
/// parts dropped, so `.` itself is the empty name of `-C`'s directory.
fn member_name(path: &str) -> Result<Vec<u8>, String> {
    if path.is_empty() {
        return Err("an empty PATH names nothing; use . for the directory".to_string());
    }
    let mut name = Vec::new();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(part) => {
                if !name.is_empty() {
                    name.push(b'/');
                }
                name.extend_from_slice(part.as_bytes());
            }
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "{path}: a PATH must be relative and may not hold .."
                ))
            }
        }
    }
    Ok(name)
}

/// Refuses a PATH whose directories, below `-C`'s, include a symlink: the
/// archive would hold the link and then a member through it, which `tar -x`
/// refuses.
fn through_no_symlink(base: &Path, name: &[u8]) -> Result<(), String> {
    let mut parent = base.to_path_buf();
    let mut parts = name.split(|byte| *byte == b'/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        parent.push(OsStr::from_bytes(part));
        if fs::symlink_metadata(&parent).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(format!(
                "{}: a PATH through this symlink is refused by tar -x; name where it points",
                parent.display()
            ));
        }
    }
    Ok(())
}

struct Packer {
    /// The archive's own file, if it is one, so it is not stored in itself.
    archive: Option<(u64, u64)>,
    buffer: Vec<u8>,
    /// A file changed while it was stored: the archive is whole, its copy not.
    changed: bool,
}

impl Packer {
    fn pack<W: Write>(
        &mut self,
        out: &mut W,
        base: &Path,
        names: &[Vec<u8>],
    ) -> Result<(), String> {
        for name in names {
            let path = if name.is_empty() {
                base.to_path_buf()
            } else {
                base.join(OsStr::from_bytes(name))
            };
            self.walk(out, path, name.clone())?;
        }
        tar_write::finish(out).map_err(|e| format!("write archive: {e}"))
    }

    /// Each member before what it holds, children in byte order. A stack, not
    /// recursion, so a deep tree costs heap rather than this thread's stack.
    fn walk<W: Write>(&mut self, out: &mut W, root: PathBuf, name: Vec<u8>) -> Result<(), String> {
        let mut stack = vec![(root, name)];
        while let Some((path, name)) = stack.pop() {
            // `-C`'s own directory, named by `.`, is followed as `cd` would be;
            // anything else is looked at, not followed.
            let looked = if name.is_empty() {
                fs::metadata(&path)
            } else {
                fs::symlink_metadata(&path)
            };
            let meta = looked.map_err(|e| format!("{}: {e}", path.display()))?;
            if self.archive == Some((meta.dev(), meta.ino())) {
                crate::emit_err(&format!(
                    "tar: {}: the archive itself is not stored\n",
                    path.display()
                ));
                continue;
            }
            let kind = meta.file_type();
            if kind.is_dir() {
                // `-C`'s directory, named by `.`, has no member of its own.
                if !name.is_empty() {
                    let mut member = name.clone();
                    member.push(b'/');
                    header(out, &member, DIRECTORY, &meta, 0, b"")?;
                }
                let mut children = fs::read_dir(&path)
                    .and_then(|entries| {
                        entries
                            .map(|entry| entry.map(|entry| entry.file_name()))
                            .collect::<io::Result<Vec<OsString>>>()
                    })
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                children.sort();
                for child in children.into_iter().rev() {
                    if std::str::from_utf8(child.as_bytes()).is_err() {
                        return Err(unreadable(&path.join(&child), "name"));
                    }
                    let mut member = name.clone();
                    if !member.is_empty() {
                        member.push(b'/');
                    }
                    member.extend_from_slice(child.as_bytes());
                    stack.push((path.join(&child), member));
                }
            } else if kind.is_symlink() {
                let target =
                    fs::read_link(&path).map_err(|e| format!("{}: {e}", path.display()))?;
                let target = target.as_os_str().as_bytes();
                if std::str::from_utf8(target).is_err() {
                    return Err(unreadable(&path, "symlink target"));
                }
                header(out, &name, SYMLINK, &meta, 0, target)?;
            } else if kind.is_file() {
                self.file(out, &path, &name, &meta)?;
            } else if kind.is_socket() {
                crate::emit_err(&format!(
                    "tar: {}: a socket is not stored\n",
                    path.display()
                ));
            } else {
                return Err(format!(
                    "{}: not a file, directory or symlink",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    /// The file as opened, which must be the one looked at, and exactly as many
    /// bytes as its header says: one cut short ends the archive with an error,
    /// and one that grew or was written meanwhile is noted. Only the final
    /// component is opened without following; directories are walked by path.
    fn file<W: Write>(
        &mut self,
        out: &mut W,
        path: &Path,
        name: &[u8],
        seen: &Metadata,
    ) -> Result<(), String> {
        let failed = |e: io::Error| format!("{}: {e}", path.display());
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK | O_NOCTTY)
            .open(path)
            .map_err(failed)?;
        let meta = file.metadata().map_err(failed)?;
        if !meta.is_file() || meta.dev() != seen.dev() || meta.ino() != seen.ino() {
            return Err(format!("{}: replaced while being stored", path.display()));
        }
        let size = meta.len();
        header(out, name, REGULAR, &meta, size, b"")?;
        let mut left = size;
        while left > 0 {
            let want = usize::try_from(left)
                .unwrap_or(usize::MAX)
                .min(self.buffer.len());
            let chunk = self.buffer.get_mut(..want).unwrap_or_default();
            let got = match file.read(chunk) {
                Ok(0) => return Err(format!("{}: shrank while being stored", path.display())),
                Ok(got) => got,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(failed(e)),
            };
            out.write_all(chunk.get(..got).unwrap_or_default())
                .map_err(|e| format!("write archive: {e}"))?;
            left -= got as u64;
        }
        let after = file.metadata().map_err(failed)?;
        if after.len() != size
            || after.mtime() != meta.mtime()
            || after.mtime_nsec() != meta.mtime_nsec()
            || after.ctime() != meta.ctime()
            || after.ctime_nsec() != meta.ctime_nsec()
        {
            crate::emit_err(&format!(
                "tar: {}: changed while being stored\n",
                path.display()
            ));
            self.changed = true;
        }
        tar_write::pad(out, size).map_err(|e| format!("write archive: {e}"))
    }
}

fn unreadable(path: &Path, what: &str) -> String {
    format!(
        "{}: its {what} is not UTF-8, which tar -x refuses",
        path.display()
    )
}

fn header<W: Write>(
    out: &mut W,
    name: &[u8],
    typeflag: u8,
    meta: &Metadata,
    size: u64,
    link: &[u8],
) -> Result<(), String> {
    let header = Header {
        name,
        typeflag,
        mode: meta.mode(),
        uid: u64::from(meta.uid()),
        gid: u64::from(meta.gid()),
        size,
        // Before 1970 is stored as 1970: ustar's octal has no sign.
        mtime: u64::try_from(meta.mtime()).unwrap_or(0),
        link,
    };
    tar_write::write_header(out, &header).map_err(|e| format!("write archive: {e}"))
}

/// The archive `-z` compresses, held in memory up to what extraction expands.
struct Bounded {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for Bounded {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(data.len()) > self.limit {
            return Err(io::Error::other(
                "the archive passes -z's 1 GiB; leave out -z",
            ));
        }
        self.bytes.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn process_umask() -> Result<u32, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|e| format!("read process umask: {e}"))?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Umask:"))
        .ok_or_else(|| "process status has no Umask field".to_string())?;
    let mask =
        u32::from_str_radix(value.trim(), 8).map_err(|e| format!("invalid process umask: {e}"))?;
    if mask > 0o777 {
        return Err("process umask exceeds 0777".to_string());
    }
    Ok(mask)
}
