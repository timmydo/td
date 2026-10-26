//! `gen_init_cpio`: the kernel's initramfs packer (`usr/gen_init_cpio.c` in
//! the pinned linux-7.2.13), so recipes pack an initramfs with a td-built
//! program instead of one the kernel build compiles and exports. It reads the
//! same spec lines and writes the same newc bytes: inodes count from 721, a
//! file's hard links share one inode and only the last carries the data,
//! `dir`/`pipe`/`sock` have nlink 2, every member names device 3:1, and the
//! trailer pads the archive to 512. `-t` and `-c` are the C program's.
//!
//! Where the C program would skip, warn or guess, this refuses, since td's
//! specs are generated: `-a` alignment, a missing `-t` (the C stamps the
//! current time), an unknown type or a line of only a type, a line with
//! leading whitespace, a separator other than space or tab, a non-ASCII or
//! control byte, a negative id (the C writes FFFFFFFF), a `nod` type other
//! than `b` or `c`, a `file` location that is not a regular file, and a
//! `${}` expansion that is not UTF-8 or does not terminate.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;

const USAGE: &str = "usage: gen_init_cpio -t TIMESTAMP [-c] [-o OUTPUT] LIST";
const HEADER: usize = 110;
const TRAILER: &str = "TRAILER!!!";
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const S_IFIFO: u32 = 0o010_000;
const S_IFSOCK: u32 = 0o140_000;
const S_IFBLK: u32 = 0o060_000;
const S_IFCHR: u32 = 0o020_000;

struct Writer<W: Write> {
    out: W,
    offset: usize,
    ino: u32,
    mtime: u32,
    csum: bool,
}

/// One header's thirteen fields after the magic, in the C program's order.
struct Header {
    ino: u32,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u32,
    size: u32,
    /// 3:1 for every member, 0:0 for the trailer.
    dev: (u32, u32),
    rdev: (u32, u32),
    namesize: u32,
    chksum: u32,
}

fn pad(offset: usize, align: usize) -> usize {
    (align - offset % align) % align
}

impl<W: Write> Writer<W> {
    fn bytes(&mut self, data: &[u8]) -> Result<(), String> {
        self.out
            .write_all(data)
            .map_err(|e| format!("write: {e}"))?;
        self.offset += data.len();
        Ok(())
    }

    fn pad_to(&mut self, align: usize) -> Result<(), String> {
        let n = pad(self.offset, align);
        self.bytes(&vec![0; n])
    }

    fn header(&mut self, h: &Header) -> Result<(), String> {
        let magic = if self.csum { "070702" } else { "070701" };
        let text = format!(
            "{magic}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}",
            h.ino,
            h.mode,
            h.uid,
            h.gid,
            h.nlink,
            h.mtime,
            h.size,
            h.dev.0,
            h.dev.1,
            h.rdev.0,
            h.rdev.1,
            h.namesize,
            h.chksum
        );
        if text.len() != HEADER {
            return Err("a header field overflows eight hex digits".into());
        }
        self.bytes(text.as_bytes())
    }

    /// The name, NUL-terminated, padded so what follows is 4-aligned.
    fn name(&mut self, name: &[u8]) -> Result<(), String> {
        self.bytes(name)?;
        self.bytes(&[0])?;
        self.pad_to(4)
    }

    fn next_ino(&mut self) -> u32 {
        let ino = self.ino;
        self.ino = self.ino.wrapping_add(1);
        ino
    }

    fn generic(
        &mut self,
        name: &str,
        mode: u32,
        ids: (u32, u32),
        nlink: u32,
        rdev: (u32, u32),
    ) -> Result<(), String> {
        let name = strip_root(name);
        let header = Header {
            ino: self.next_ino(),
            mode,
            uid: ids.0,
            gid: ids.1,
            nlink,
            mtime: self.mtime,
            size: 0,
            dev: (3, 1),
            rdev,
            namesize: namesize(name)?,
            chksum: 0,
        };
        self.header(&header)?;
        self.name(name.as_bytes())
    }

    fn slink(
        &mut self,
        name: &str,
        target: &str,
        mode: u32,
        ids: (u32, u32),
    ) -> Result<(), String> {
        let name = strip_root(name);
        let header = Header {
            ino: self.next_ino(),
            mode: S_IFLNK | mode,
            uid: ids.0,
            gid: ids.1,
            nlink: 1,
            mtime: self.mtime,
            size: namesize(target)?,
            dev: (3, 1),
            rdev: (0, 0),
            namesize: namesize(name)?,
            chksum: 0,
        };
        self.header(&header)?;
        self.name(name.as_bytes())?;
        self.bytes(target.as_bytes())?;
        self.bytes(&[0])?;
        self.pad_to(4)
    }

    fn file(
        &mut self,
        names: &[&str],
        location: &str,
        mode: u32,
        ids: (u32, u32),
    ) -> Result<(), String> {
        let mut file = std::fs::File::open(location).map_err(|e| format!("{location}: {e}"))?;
        // The C copies st_size bytes; a FIFO or device has no such size.
        let meta = file.metadata().map_err(|e| format!("{location}: {e}"))?;
        if !meta.is_file() {
            return Err(format!("{location}: not a regular file"));
        }
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|e| format!("{location}: {e}"))?;
        let size = u32::try_from(data.len())
            .map_err(|_| format!("{location}: Size exceeds maximum cpio file size"))?;
        let chksum = if self.csum {
            data.iter()
                .fold(0u32, |sum, b| sum.wrapping_add(u32::from(*b)))
        } else {
            0
        };
        let nlink = u32::try_from(names.len()).map_err(|_| "too many links")?;
        let last = names.len().saturating_sub(1);
        for (n, name) in names.iter().enumerate() {
            let name = strip_root(name);
            let carries = n == last;
            let header = Header {
                ino: self.ino,
                mode: S_IFREG | mode,
                uid: ids.0,
                gid: ids.1,
                nlink,
                mtime: self.mtime,
                size: if carries { size } else { 0 },
                dev: (3, 1),
                rdev: (0, 0),
                namesize: namesize(name)?,
                chksum: if carries && size > 0 { chksum } else { 0 },
            };
            self.header(&header)?;
            self.name(name.as_bytes())?;
            if carries {
                self.bytes(&data)?;
                self.pad_to(4)?;
            }
        }
        self.next_ino();
        Ok(())
    }

    fn trailer(&mut self) -> Result<(), String> {
        let header = Header {
            ino: 0,
            mode: 0,
            uid: 0,
            gid: 0,
            nlink: 1,
            mtime: 0,
            size: 0,
            dev: (0, 0),
            rdev: (0, 0),
            namesize: namesize(TRAILER)?,
            chksum: 0,
        };
        self.header(&header)?;
        self.name(TRAILER.as_bytes())?;
        self.pad_to(512)?;
        self.out.flush().map_err(|e| format!("write: {e}"))
    }
}

fn strip_root(name: &str) -> &str {
    name.strip_prefix('/').unwrap_or(name)
}

fn namesize(name: &str) -> Result<u32, String> {
    u32::try_from(name.len() + 1).map_err(|_| format!("{name}: name too long"))
}

fn octal(field: &str) -> Result<u32, String> {
    u32::from_str_radix(field, 8).map_err(|_| format!("'{field}' is not an octal mode"))
}

fn id(field: &str) -> Result<u32, String> {
    field
        .parse::<u32>()
        .map_err(|_| format!("'{field}' is not a non-negative id"))
}

/// `${NAME}` in a file location expands from the environment, empty when
/// unset. As in the C, the result is rescanned, so an expansion may itself
/// expand; the rescans are bounded rather than left to loop.
fn expand(location: &str) -> Result<String, String> {
    let mut text = location.to_string();
    for _ in 0..64 {
        let Some(start) = text.find("${") else {
            return Ok(text);
        };
        let Some(len) = text.get(start + 2..).and_then(|tail| tail.find('}')) else {
            return Ok(text);
        };
        let name = text.get(start + 2..start + 2 + len).unwrap_or_default();
        let value = match std::env::var_os(name) {
            Some(value) => value
                .into_string()
                .map_err(|_| format!("${{{name}}} is not UTF-8"))?,
            None => String::new(),
        };
        let head = text.get(..start).unwrap_or_default();
        let tail = text.get(start + 3 + len..).unwrap_or_default();
        text = format!("{head}{value}{tail}");
    }
    Err(format!("{location}: expansion does not terminate"))
}

fn line<W: Write>(w: &mut Writer<W>, text: &str) -> Result<(), String> {
    if text.starts_with([' ', '\t']) {
        return Err(format!("leading whitespace in '{text}'"));
    }
    if let Some(bad) = text
        .bytes()
        .find(|b| !b.is_ascii_graphic() && *b != b' ' && *b != b'\t')
    {
        return Err(format!("byte {bad:#04x} in '{}'", text.escape_debug()));
    }
    let fields: Vec<&str> = text.split([' ', '\t']).filter(|f| !f.is_empty()).collect();
    let Some((kind, args)) = fields.split_first() else {
        return Ok(());
    };
    let ids = |uid: &str, gid: &str| Ok::<_, String>((id(uid)?, id(gid)?));
    match (*kind, args) {
        ("file", [name, location, mode, uid, gid, links @ ..]) => {
            let mut names = vec![*name];
            names.extend_from_slice(links);
            w.file(&names, &expand(location)?, octal(mode)?, ids(uid, gid)?)
        }
        ("dir", [name, mode, uid, gid, ..]) => {
            w.generic(name, S_IFDIR | octal(mode)?, ids(uid, gid)?, 2, (0, 0))
        }
        ("pipe", [name, mode, uid, gid, ..]) => {
            w.generic(name, S_IFIFO | octal(mode)?, ids(uid, gid)?, 2, (0, 0))
        }
        ("sock", [name, mode, uid, gid, ..]) => {
            w.generic(name, S_IFSOCK | octal(mode)?, ids(uid, gid)?, 2, (0, 0))
        }
        ("nod", [name, mode, uid, gid, kind, major, minor, ..]) => {
            let kind = match *kind {
                "b" => S_IFBLK,
                "c" => S_IFCHR,
                other => return Err(format!("nod type '{other}' is neither b nor c")),
            };
            let rdev = (id(major)?, id(minor)?);
            w.generic(name, kind | octal(mode)?, ids(uid, gid)?, 1, rdev)
        }
        ("slink", [name, target, mode, uid, gid, ..]) => {
            w.slink(name, target, octal(mode)?, ids(uid, gid)?)
        }
        _ => Err(format!("unrecognised spec line '{text}'")),
    }
}

/// Pack `list` (the spec text) to `out` with every mtime `mtime`.
fn pack<W: Write>(list: &str, mtime: u32, csum: bool, out: W) -> Result<(), String> {
    let mut w = Writer {
        out,
        offset: 0,
        ino: 721,
        mtime,
        csum,
    };
    for (n, text) in list.lines().enumerate() {
        if text.starts_with('#') {
            continue;
        }
        line(&mut w, text).map_err(|e| format!("{e} (line {})", n + 1))?;
    }
    w.trailer()
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut mtime: Option<u32> = None;
    let (mut csum, mut output, mut list) = (false, None, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-t" => {
                let value = it.next().ok_or(USAGE)?;
                mtime = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| format!("Invalid timestamp: {value}"))?,
                );
            }
            "-c" => csum = true,
            "-o" => output = Some(it.next().ok_or(USAGE)?.clone()),
            flag if flag.starts_with('-') && flag != "-" => {
                return Err(format!("unrecognised option '{flag}'\n{USAGE}"));
            }
            path if list.is_none() => list = Some(path.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    // Without -t the C program stamps the current time; td's archives are
    // reproducible, so the timestamp is required.
    let mtime = mtime.ok_or(USAGE)?;
    let list = list.ok_or(USAGE)?;
    let mut text = String::new();
    if list == "-" {
        std::io::stdin().read_to_string(&mut text)
    } else {
        std::fs::File::open(&list).and_then(|mut f| f.read_to_string(&mut text))
    }
    .map_err(|e| format!("{list}: {e}"))?;
    match output {
        Some(path) => {
            // 0600, as the C opens it.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| format!("{path}: {e}"))?;
            pack(&text, mtime, csum, std::io::BufWriter::new(file))?;
        }
        None => pack(
            &text,
            mtime,
            csum,
            std::io::BufWriter::new(std::io::stdout().lock()),
        )?,
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn packed(list: &str) -> Vec<u8> {
        let mut out = Vec::new();
        pack(list, 1, false, &mut out).unwrap();
        out
    }

    #[test]
    fn the_archive_reads_back_with_the_listing_reader() {
        let dir = td_fs::TempDir::new("gen-init-cpio").unwrap();
        let file = dir.join("init");
        std::fs::write(&file, b"#!/bin/sh\n").unwrap();
        let list = format!(
            "# comment\n\ndir /dev 0755 0 0\nnod /dev/console 0600 0 0 c 5 1\n\
             slink /bin/sh td-sh 0777 0 0\nfile /init {} 0755 0 0 /linit\n",
            file.display()
        );
        let out = packed(&list);
        assert_eq!(out.len() % 512, 0);
        let members = crate::cpio::members(&out).unwrap();
        let names: Vec<&[u8]> = members.iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            [&b"dev"[..], b"dev/console", b"bin/sh", b"init", b"linit"]
        );
        let inos: Vec<u32> = members.iter().map(|m| m.ino).collect();
        assert_eq!(inos, [721, 722, 723, 724, 724], "hard links share an inode");
        assert!(members.iter().all(|m| m.mtime == 1));
        assert_eq!(members[3].data, b"");
        assert_eq!(
            members[4].data, b"#!/bin/sh\n",
            "the last link carries the data"
        );
        assert_eq!(members[4].nlink, 2);
        assert_eq!(members[2].data, b"td-sh\0");
    }

    #[test]
    fn the_header_is_the_c_programs_byte_for_byte() {
        let out = packed("dir /x 0755 0 0\n");
        let header = std::str::from_utf8(&out[..HEADER]).unwrap();
        let fields = [
            "000002D1", "000041ED", "00000000", "00000000", "00000002", "00000001", "00000000",
            "00000003", "00000001", "00000000", "00000000", "00000002", "00000000",
        ];
        assert_eq!(header, format!("070701{}", fields.concat()));
        // 110 + "x\0" is already 4-aligned: the trailer follows at once.
        assert_eq!(&out[HEADER..HEADER + 2], b"x\0");
        assert_eq!(&out[HEADER + 2..HEADER + 8], b"070701");
    }

    #[test]
    fn locations_expand_and_bad_specs_are_refused() {
        assert_eq!(expand("a/b").unwrap(), "a/b");
        assert_eq!(expand("${TD_GEN_INIT_CPIO_UNSET_VAR}/x").unwrap(), "/x");
        assert_eq!(expand("${unterminated").unwrap(), "${unterminated");
        for bad in [
            "dir /x 0755 0\n",
            "dir /x 0758 0 0\n",
            "dir /x 0755 -1 0\n",
            "fifo /x 0755 0 0\n",
            "dir\n",
            " dir /x 0755 0 0\n",
            "dir /x\x0b0755 0 0\n",
            "dir /a\0b 0755 0 0\n",
            "dir /\u{e9} 0755 0 0\n",
            "nod /x 0600 0 0 block 1 2\n",
            "file /x /dev/null 0644 0 0\n",
        ] {
            let mut out = Vec::new();
            assert!(pack(bad, 1, false, &mut out).is_err(), "{bad}");
        }
        assert!(run(&["x".into()]).is_err(), "the timestamp is required");
        assert!(run(&["-a".into(), "4".into(), "x".into()]).is_err());
    }

    /// `usr/gen_init_cpio -t 1 [-c]` of linux-7.2.13, compiled from the pinned
    /// source, on the spec below; hex, with the zero block padding after the
    /// trailer trimmed.
    const GOLDEN_PLAIN: &[&str] = &[
        "30373037303130303030303244313030303034314544303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303034303030303030303064657600000030373037",
        "30313030303030324432303030303231383030303030303030303030303030303030303030303030",
        "30313030303030303031303030303030303030303030303030333030303030303031303030303030",
        "30353030303030303031303030303030304330303030303030306465762f636f6e736f6c65000000",
        "30373037303130303030303244333030303036314230303030303030303030303030303030363030",
        "30303030303130303030303030313030303030303030303030303030303330303030303030313030",
        "3030303046453030303030303030303030303030303830303030303030306465762f766461000000",
        "30373037303130303030303244343030303041314646303030303030303030303030303030303030",
        "30303030303130303030303030313030303030303036303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303037303030303030303062696e2f736800000000",
        "74642d73680000003037303730313030303030324435303030303831454430303030303030303030",
        "30303030303030303030303030323030303030303031303030303030303030303030303030333030",
        "3030303030313030303030303030303030303030303030303030303030353030303030303030696e",
        "69740000303730373031303030303032443530303030383145443030303030303030303030303030",
        "30303030303030303032303030303030303130303030303030373030303030303033303030303030",
        "303130303030303030303030303030303030303030303030303630303030303030306c696e697400",
        "68656c6c6f0a21003037303730313030303030324436303030303831413430303030303345383030",
        "30303033453830303030303030313030303030303031303030303030303030303030303030333030",
        "30303030303130303030303030303030303030303030303030303030303230303030303030306500",
        "30373037303130303030303244373030303031313830303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303039303030303030303072756e2f6669666f0000",
        "30373037303130303030303244383030303043313830303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303039303030303030303072756e2f736f636b0000",
        "30373037303130303030303030303030303030303030303030303030303030303030303030303030",
        "30303030303130303030303030303030303030303030303030303030303030303030303030303030",
        "303030303030303030303030303030303030303030423030303030303030545241494c4552212121",
        "00000000",
    ];
    const GOLDEN_CSUM: &[&str] = &[
        "30373037303230303030303244313030303034314544303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303034303030303030303064657600000030373037",
        "30323030303030324432303030303231383030303030303030303030303030303030303030303030",
        "30313030303030303031303030303030303030303030303030333030303030303031303030303030",
        "30353030303030303031303030303030304330303030303030306465762f636f6e736f6c65000000",
        "30373037303230303030303244333030303036314230303030303030303030303030303030363030",
        "30303030303130303030303030313030303030303030303030303030303330303030303030313030",
        "3030303046453030303030303030303030303030303830303030303030306465762f766461000000",
        "30373037303230303030303244343030303041314646303030303030303030303030303030303030",
        "30303030303130303030303030313030303030303036303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303037303030303030303062696e2f736800000000",
        "74642d73680000003037303730323030303030324435303030303831454430303030303030303030",
        "30303030303030303030303030323030303030303031303030303030303030303030303030333030",
        "3030303030313030303030303030303030303030303030303030303030353030303030303030696e",
        "69740000303730373032303030303032443530303030383145443030303030303030303030303030",
        "30303030303030303032303030303030303130303030303030373030303030303033303030303030",
        "303130303030303030303030303030303030303030303030303630303030303233466c696e697400",
        "68656c6c6f0a21003037303730323030303030324436303030303831413430303030303345383030",
        "30303033453830303030303030313030303030303031303030303030303030303030303030333030",
        "30303030303130303030303030303030303030303030303030303030303230303030303030306500",
        "30373037303230303030303244373030303031313830303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303039303030303030303072756e2f6669666f0000",
        "30373037303230303030303244383030303043313830303030303030303030303030303030303030",
        "30303030303230303030303030313030303030303030303030303030303330303030303030313030",
        "30303030303030303030303030303030303030303039303030303030303072756e2f736f636b0000",
        "30373037303230303030303030303030303030303030303030303030303030303030303030303030",
        "30303030303130303030303030303030303030303030303030303030303030303030303030303030",
        "303030303030303030303030303030303030303030423030303030303030545241494c4552212121",
        "00000000",
    ];

    fn unhex(lines: &[&str]) -> Vec<u8> {
        let text = lines.concat();
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn the_archive_is_the_c_programs_byte_for_byte() {
        let dir = td_fs::TempDir::new("gen-init-cpio-golden").unwrap();
        std::fs::write(dir.join("odd"), b"hello\n!").unwrap();
        std::fs::write(dir.join("empty"), b"").unwrap();
        let d = dir.path().display();
        let spec = format!(
            "dir /dev 0755 0 0\n\
             nod /dev/console 0600 0 0 c 5 1\n\
             nod /dev/vda 0660 0 6 b 254 0\n\
             slink /bin/sh td-sh 0777 0 0\n\
             file /init {d}/odd 0755 0 0 /linit\n\
             file /e {d}/empty 0644 1000 1000\n\
             pipe /run/fifo 0600 0 0\n\
             sock /run/sock 0600 0 0\n"
        );
        for (csum, golden) in [(false, GOLDEN_PLAIN), (true, GOLDEN_CSUM)] {
            let mut out = Vec::new();
            pack(&spec, 1, csum, &mut out).unwrap();
            let golden = unhex(golden);
            assert_eq!(out.len(), 1536);
            assert_eq!(&out[..golden.len()], &golden[..], "csum {csum}");
            assert!(out[golden.len()..].iter().all(|b| *b == 0));
        }
    }
}
