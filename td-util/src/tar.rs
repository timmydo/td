//! Extraction-only tar frontend over the engine's bounded archive reader.
use std::fs::File;
use std::io::{BufReader, Cursor};
use std::path::Path;

const MAX_ENTRY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_GZIP_BYTES: usize = 1024 * 1024 * 1024;
const HELP: &str = "Usage: tar -x [-z] -f ARCHIVE [-C DIRECTORY]\n\
Extract a tar or gzip-compressed tar archive. Combined flags (-xzf) are supported.\n\
Only extraction is supported; unsupported options are errors.\n\
Limits: 1 GiB per file, 16 GiB total payload, 1 million entries.\n\
Gzip limits: 257 MiB compressed, 1 GiB expanded (decoded in memory).\n\
Modes honor the process umask; special permission bits are stripped.\n\
Directory modes and mtimes are restored after extraction.\n\
Paths may not escape the destination or traverse symlink ancestors.\n\
Extraction may leave partial output on error; use a private destination.\n";

pub fn run(args: &[String]) -> Result<u8, String> {
    if args.len() == 1 && args.first().is_some_and(|s| s == "--help") {
        crate::emit(HELP)?;
        return Ok(0);
    }
    let mut extract = false;
    let mut gzip = false;
    let mut archive = None;
    let mut directory = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let flags = arg
            .strip_prefix('-')
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("unsupported operand {arg}; see tar --help"))?;
        for (offset, flag) in flags.char_indices() {
            match flag {
                'x' if !extract => extract = true,
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
    if !extract {
        return Err("-x is required; see tar --help".to_string());
    }
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
