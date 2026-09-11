//! Control-plane preparation of the pinned x86 Linux UAPI seed.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

type Result<T> = std::result::Result<T, String>;
const MAX_SOURCE: u64 = 512 * 1024 * 1024;
const MAX_HEADER: u64 = 2 * 1024 * 1024;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
const POLICY: &[(&str, &str)] = &[
    (
        "scripts/headers_install.sh",
        "498539bbb3c7b6633d24d6e0ae8d13aeaf0e017c7fe491cc04968fa98bb93e12",
    ),
    (
        "scripts/Makefile.headersinst",
        "1bbac7d3f44078a37840b2b4e3cc6f0339576c19704ebaa3c15159c6817fbb62",
    ),
    (
        "scripts/Makefile.asm-generic",
        "3f9e9b2396bbc28b3bed736d2c452768392da0f9911df776dc22cac40f5a6dbd",
    ),
    (
        "arch/x86/include/uapi/asm/Kbuild",
        "d7743fcfe7ee5a906c46f3ab026d0158e7a812de65904c72e23ec37e6d368500",
    ),
    (
        "include/uapi/linux/Kbuild",
        "4d8fde974c06a5972a01b35c006499068667e0e0f10770ec9cc2bf22057c9509",
    ),
    (
        "include/uapi/asm-generic/Kbuild.asm",
        "e1d9abab91ade3a7bd93472cf5dd825305b111e5f5eeb3b5d0cbb7067bebea2d",
    ),
    (
        "arch/x86/entry/syscalls/syscallhdr.sh",
        "286cea4202bdc67fcf77fec051e444dce29a9e23fd34c267c780a4552f50b7c3",
    ),
    (
        "arch/x86/entry/syscalls/Makefile",
        "f7857161e29f29f19449b479cc751bb5636849805d79fc9c12230fef2351a6a0",
    ),
];

struct Work(PathBuf);
impl Drop for Work {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} exceeds {limit} bytes", path.display()));
    }
    Ok(bytes)
}

fn verify_policy(source: &Path) -> Result<()> {
    for (relative, expected) in POLICY {
        let bytes = bounded(&source.join(relative), MAX_HEADER)?;
        if crate::sha256::hex_digest(&bytes) != *expected {
            return Err(format!("unsupported kernel header policy: {relative}"));
        }
    }
    Ok(())
}

fn collect(
    base: &Path,
    directory: &Path,
    depth: usize,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    if depth > 32 || files.len() > 2048 {
        return Err("kernel header tree exceeds its bounds".into());
    }
    for entry in
        fs::read_dir(directory).map_err(|e| format!("read {}: {e}", directory.display()))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| e.to_string())?;
        if kind.is_dir() {
            collect(base, &path, depth + 1, files)?;
        } else if !kind.is_file() {
            return Err(format!("non-regular UAPI source: {}", path.display()));
        } else if matches!(path.extension().and_then(|s| s.to_str()), Some("h" | "agh")) {
            let name = path
                .strip_prefix(base)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non-UTF-8 header name")?
                .to_string();
            if files.len() >= 2048 {
                return Err("kernel header tree exceeds 2048 files".into());
            }
            let bytes = bounded(&path, MAX_HEADER)?;
            if files.values().map(Vec::len).sum::<usize>() + bytes.len() > MAX_TOTAL {
                return Err("kernel headers exceed 64 MiB".into());
            }
            if files.insert(name.clone(), bytes).is_some() {
                return Err(format!("duplicate UAPI header: {name}"));
            }
        } else if path.file_name().and_then(|s| s.to_str()) == Some("Kbuild")
            && !matches!(
                path.strip_prefix(base).ok().and_then(|p| p.to_str()),
                Some("linux/Kbuild" | "asm/Kbuild")
            )
        {
            return Err(format!("unsupported UAPI selection: {}", path.display()));
        }
    }
    if files.len() > 2048 || files.values().map(Vec::len).sum::<usize>() > MAX_TOTAL {
        return Err("kernel headers exceed 64 MiB".into());
    }
    Ok(())
}

fn syscall_header(
    source: &[u8],
    name: &str,
    abis: &[&str],
    offset: Option<&str>,
) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(source).map_err(|e| e.to_string())?;
    let mut calls = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(number) = fields.first() else {
            continue;
        };
        if number.starts_with('#') {
            continue;
        }
        let number = number
            .parse::<u32>()
            .map_err(|_| "invalid syscall number")?;
        let abi = fields.get(1).ok_or("syscall has no ABI")?;
        let call = fields.get(2).ok_or("syscall has no name")?;
        if !call.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err("invalid syscall name".into());
        }
        if abis.contains(abi) && calls.insert(number, *call).is_some() {
            return Err("duplicate syscall number".into());
        }
    }
    let guard = format!("_ASM_X86_{}", name.to_ascii_uppercase().replace('.', "_"));
    let mut out = format!("#ifndef {guard}\n#define {guard} 1\n\n");
    for (number, call) in calls {
        let value = match offset {
            Some(offset) => format!("({offset} + {number})"),
            None => number.to_string(),
        };
        out.push_str(&format!("#define __NR_{call} {value}\n"));
    }
    out.push_str(&format!("\n#endif /* {guard} */\n"));
    Ok(out.into_bytes())
}

fn white(b: u8) -> bool {
    matches!(b, b' ' | b'\t')
}
fn white_paren(b: u8) -> bool {
    white(b) || b == b'('
}
fn non_alnum(b: u8) -> bool {
    !b.is_ascii_alphanumeric()
}
fn non_word(b: u8) -> bool {
    non_alnum(b) && b != b'_'
}

// Keep the consumed boundary bytes: sed's global substitutions do not reuse
// a trailing boundary as the next match's leading boundary.
type Boundary = fn(u8) -> bool;

fn substitute(
    input: &[u8],
    tokens: &[&[u8]],
    leading: (Option<Boundary>, bool),
    trailing: (Boundary, bool),
    replacement: fn(&[u8]) -> Vec<u8>,
    keep_suffix: bool,
) -> Vec<u8> {
    let (prefix, start) = leading;
    let (suffix, end) = trailing;
    let mut out = Vec::with_capacity(input.len());
    let mut at = 0;
    while let Some(byte) = input.get(at).copied() {
        let mut matched = None;
        for leading in [0, 1] {
            let allowed = if leading == 0 {
                prefix.is_none() || (start && at == 0)
            } else {
                prefix.is_some_and(|test| test(byte))
            };
            if !allowed {
                continue;
            }
            for token in tokens {
                let after = at + leading + token.len();
                if input.get(at + leading..after) != Some(*token) {
                    continue;
                }
                let trailing = match input.get(after).copied() {
                    None if end => 0,
                    Some(b) if suffix(b) => 1,
                    _ => continue,
                };
                matched = Some((leading, *token, after, trailing));
                break;
            }
            if matched.is_some() {
                break;
            }
        }
        if let Some((leading, token, after, trailing)) = matched {
            if leading == 1 {
                out.push(byte);
            }
            out.extend(replacement(token));
            if keep_suffix && trailing == 1 {
                if let Some(byte) = input.get(after) {
                    out.push(*byte);
                }
            }
            at = after + trailing;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    out
}

fn remove_uapi_guard(line: &[u8]) -> Vec<u8> {
    for at in 0..line.len() {
        if line.get(at) != Some(&b'#') {
            continue;
        }
        let rest = line.get(at + 1..).unwrap_or(&[]);
        let mut keyword = if rest.starts_with(b"ifndef") || rest.starts_with(b"define") {
            6
        } else if rest.starts_with(b"endif") {
            5
        } else {
            continue;
        };
        if keyword == 5 {
            while rest.get(keyword).is_some_and(|b| white(*b)) {
                keyword += 1;
            }
            if rest.get(keyword..keyword + 2) != Some(b"/*") {
                continue;
            }
            keyword += 2;
        }
        let mut tail = keyword;
        while rest.get(tail).is_some_and(|b| white(*b)) {
            tail += 1;
        }
        if rest.get(tail..tail + 5) == Some(b"_UAPI") {
            let mut out = line.get(..at + 1 + keyword).unwrap_or(&[]).to_vec();
            out.push(b' ');
            out.extend_from_slice(rest.get(tail + 5..).unwrap_or(&[]));
            return out;
        }
    }
    line.to_vec()
}

fn sanitize(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    for part in input.split_inclusive(|b| *b == b'\n') {
        let (line, newline) = match part.strip_suffix(b"\n") {
            Some(line) => (line, true),
            None => (part, false),
        };
        let line = substitute(
            line,
            &[b"__user", b"__force", b"__iomem"],
            (Some(white_paren), false),
            (white, false),
            |_| Vec::new(),
            false,
        );
        let line = substitute(
            &line,
            &[b"__attribute_const__"],
            (None, false),
            (white, true),
            |_| Vec::new(),
            true,
        );
        let line = line
            .strip_prefix(b"#include <linux/compiler.h>")
            .or_else(|| line.strip_prefix(b"#include <linux/compiler_types.h>"))
            .unwrap_or(&line);
        let line = substitute(
            line,
            &[b"__packed"],
            (Some(non_alnum), true),
            (non_word, true),
            |_| b"__attribute__((packed))".to_vec(),
            true,
        );
        let line = substitute(
            &line,
            &[b"inline", b"asm", b"volatile"],
            (Some(white_paren), true),
            (white_paren, true),
            |word| {
                let mut out = b"__".to_vec();
                out.extend_from_slice(word);
                out.extend_from_slice(b"__");
                out
            },
            true,
        );
        out.extend(remove_uapi_guard(&line));
        if newline {
            out.push(b'\n');
        }
    }
    out
}

fn put(header: &mut [u8], at: usize, bytes: &[u8]) -> Result<()> {
    header
        .get_mut(at..at + bytes.len())
        .ok_or("tar field exceeds header")?
        .copy_from_slice(bytes);
    Ok(())
}

fn entry(output: &mut impl Write, name: &str, data: &[u8], directory: bool) -> Result<usize> {
    if name.len() >= 100 || !name.is_ascii() {
        return Err("kernel header tar name exceeds the supported GNU form".into());
    }
    let mut header = [0u8; 512];
    put(&mut header, 0, name.as_bytes())?;
    put(
        &mut header,
        100,
        if directory {
            b"0000755\0"
        } else {
            b"0000644\0"
        },
    )?;
    put(&mut header, 108, b"0000000\0")?;
    put(&mut header, 116, b"0000000\0")?;
    put(
        &mut header,
        124,
        format!("{:011o}\0", data.len()).as_bytes(),
    )?;
    put(&mut header, 136, b"00000000000\0")?;
    put(&mut header, 148, b"        ")?;
    put(&mut header, 156, if directory { b"5" } else { b"0" })?;
    put(&mut header, 257, b"ustar  \0")?;
    let sum: u64 = header.iter().map(|b| u64::from(*b)).sum();
    put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes())?;
    output
        .write_all(&header)
        .and_then(|()| output.write_all(data))
        .map_err(|e| e.to_string())?;
    let padding = (512 - data.len() % 512) % 512;
    output
        .write_all([0u8; 512].get(..padding).ok_or("invalid tar padding")?)
        .map_err(|e| e.to_string())?;
    Ok(512 + data.len() + padding)
}

fn pack_dir(
    output: &mut impl Write,
    files: &BTreeMap<String, Vec<u8>>,
    prefix: &str,
) -> Result<usize> {
    let mut written = entry(output, &format!("./{prefix}"), &[], true)?;
    let mut children = BTreeSet::new();
    for name in files.keys() {
        if let Some(rest) = name.strip_prefix(prefix) {
            if let Some(child) = rest.split('/').next() {
                children.insert(child);
            }
        }
    }
    for child in children {
        let path = format!("{prefix}{child}");
        written += match files.get(&path) {
            Some(bytes) => entry(output, &format!("./{path}"), bytes, false)?,
            None => pack_dir(output, files, &format!("{path}/"))?,
        };
    }
    Ok(written)
}

fn pack(output: &mut impl Write, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    let written = pack_dir(output, files, "")?;
    let zeros = 1024 + (10240 - (written + 1024) % 10240) % 10240;
    output
        .write_all(
            [0u8; 11264]
                .get(..zeros)
                .ok_or("invalid final tar padding")?,
        )
        .and_then(|()| output.flush())
        .map_err(|e| e.to_string())
}

pub(crate) fn run_cli(args: &[String]) -> Result<()> {
    let [archive, checksum, arch, work] = args else {
        return Err("usage: kernel-headers SOURCE SHA256 i386|x86_64 NEW-WORK-DIR".into());
    };
    if !matches!(arch.as_str(), "i386" | "x86_64")
        || Path::new(archive).file_name().and_then(|s| s.to_str()) != Some("linux-4.14.67.tar.xz")
        || checksum.len() != 64
        || !checksum
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
    {
        return Err("unsupported kernel header source, digest or architecture".into());
    }
    let work_path = PathBuf::from(work);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(work)
        .map_err(|e| format!("create new header work: {e}"))?;
    let mut work = Work(work_path);
    work.0 = work.0.canonicalize().map_err(|e| e.to_string())?;
    let bytes = bounded(Path::new(archive), MAX_SOURCE)?;
    if crate::sha256::hex_digest(&bytes) != *checksum {
        return Err("kernel source SHA-256 mismatch".into());
    }
    let private_archive = work.0.join("source.tar.xz");
    fs::write(&private_archive, bytes).map_err(|e| e.to_string())?;
    let source = work.0.join("source");
    crate::tar::unpack_archive(&private_archive, &source, false)?;
    verify_policy(&source)?;
    let unifdef = work.0.join("unifdef");
    let compiler = std::env::var_os("HOSTCC")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gcc".into());
    let status = Command::new(compiler)
        .args(["-std=gnu89", "-O2", "-o"])
        .arg(&unifdef)
        .arg(source.join("scripts/unifdef.c"))
        .stdin(Stdio::inherit())
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("compile pinned unifdef: {e}"))?;
    if !status.success() {
        return Err(format!("compile pinned unifdef: {status}"));
    }
    let mut files = BTreeMap::new();
    let common = source.join("include/uapi");
    let x86 = source.join("arch/x86/include/uapi");
    collect(&common, &common, 0, &mut files)?;
    collect(&x86, &x86, 0, &mut files)?;
    for name in ["a.out.h", "kvm.h", "kvm_para.h"] {
        if !files.contains_key(&format!("asm/{name}")) {
            files.remove(&format!("linux/{name}"));
        }
    }
    for (name, table, abis, offset) in [
        ("unistd_32.h", "syscall_32.tbl", ["i386"].as_slice(), None),
        (
            "unistd_64.h",
            "syscall_64.tbl",
            ["common", "64"].as_slice(),
            None,
        ),
        (
            "unistd_x32.h",
            "syscall_64.tbl",
            ["common", "x32"].as_slice(),
            Some("__X32_SYSCALL_BIT"),
        ),
    ] {
        let path = source.join("arch/x86/entry/syscalls").join(table);
        let bytes = syscall_header(&bounded(&path, MAX_HEADER)?, name, abis, offset)?;
        // Upstream excludes source headers from the generated-header list.
        files.entry(format!("asm/{name}")).or_insert(bytes);
    }
    let temporary = work.0.join("sanitized.h");
    for (name, bytes) in &mut files {
        fs::write(&temporary, sanitize(bytes)).map_err(|e| e.to_string())?;
        let result = Command::new(&unifdef)
            .args(["-U__KERNEL__", "-D__EXPORTED_HEADERS__"])
            .arg(&temporary)
            .stdin(Stdio::inherit())
            .output()
            .map_err(|e| format!("unifdef {name}: {e}"))?;
        if !matches!(result.status.code(), Some(0 | 1)) || result.stdout.len() as u64 > MAX_HEADER {
            return Err(format!(
                "unifdef {name}: {}: {}",
                result.status,
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        *bytes = result.stdout;
    }
    files.insert("linux/version.h".into(), b"#define LINUX_VERSION_CODE 265795\n#define KERNEL_VERSION(a,b,c) (((a) << 16) + ((b) << 8) + (c))\n".to_vec());
    if files.len() > 2048 || files.values().map(Vec::len).sum::<usize>() > MAX_TOTAL {
        return Err("generated headers exceed their bounds".into());
    }
    pack(&mut std::io::stdout().lock(), &files)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    #[test]
    fn output_flush_errors_refuse_success() {
        struct FlushError;
        impl Write for FlushError {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::other("flush fixture"))
            }
        }
        assert!(pack(&mut FlushError, &BTreeMap::new())
            .unwrap_err()
            .contains("flush fixture"));
    }
    #[test]
    fn unsupported_inputs_leave_existing_work_untouched() {
        let dir = std::env::temp_dir().join(format!("td-header-refusal-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("owned"), b"keep").unwrap();
        let args = vec![
            "linux-4.14.67.tar.xz".into(),
            "0".repeat(64),
            "x86_64".into(),
            dir.display().to_string(),
        ];
        assert!(run_cli(&args)
            .unwrap_err()
            .contains("create new header work"));
        assert_eq!(fs::read(dir.join("owned")).unwrap(), b"keep");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn policy_changes_are_refused_before_compiling() {
        let dir = std::env::temp_dir().join(format!("td-header-policy-{}", std::process::id()));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("scripts")).unwrap();
        fs::write(dir.join("scripts/headers_install.sh"), b"altered").unwrap();
        assert!(verify_policy(&dir)
            .unwrap_err()
            .contains("unsupported kernel header policy"));
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn sanitization_preserves_sed_boundary_consumption() {
        assert_eq!(
            sanitize(b"inline inline\n#define _UAPI_TEST\nint __user *p;\n__packed x;\n"),
            b"__inline__ inline\n#define _TEST\nint *p;\n__attribute__((packed)) x;\n"
        );
        assert_eq!(
            sanitize(b"foo___packed x;\n"),
            b"foo___attribute__((packed)) x;\n"
        );
        assert_eq!(
            sanitize(b"#include <linux/compiler_types.h>\n__attribute_const__\n"),
            b"\n\n"
        );
    }
    #[test]
    fn syscall_headers_select_sort_and_offset_exact_abis() {
        let bytes = syscall_header(
            b"2 x32 second entry\n1 common first entry\n3 64 excluded entry\n",
            "unistd_x32.h",
            &["common", "x32"],
            Some("__X32_SYSCALL_BIT"),
        )
        .unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), "#ifndef _ASM_X86_UNISTD_X32_H\n#define _ASM_X86_UNISTD_X32_H 1\n\n#define __NR_first (__X32_SYSCALL_BIT + 1)\n#define __NR_second (__X32_SYSCALL_BIT + 2)\n\n#endif /* _ASM_X86_UNISTD_X32_H */\n");
    }
    #[test]
    fn archive_uses_gnu_directory_order_and_fixed_block_padding() {
        let files = BTreeMap::from([
            ("asm/z.h".into(), b"z".to_vec()),
            ("asm-generic/a.h".into(), b"a".to_vec()),
        ]);
        let mut bytes = Vec::new();
        pack(&mut bytes, &files).unwrap();
        assert_eq!(bytes.len(), 10240);
        assert_eq!(&bytes[..2], b"./");
        assert_eq!(&bytes[512..518], b"./asm/");
        assert_eq!(&bytes[2048..2062], b"./asm-generic/");
        assert_eq!(&bytes[257..265], b"ustar  \0");
    }
}
