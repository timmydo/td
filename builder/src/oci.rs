//! oci.rs — td-native, zero-dep, DETERMINISTIC docker-archive (OCI image) writer.
//!
//! This is brick 1 of the system-image-native track (plan/system-image-native.md):
//! begin retiring the LAST Guile build lowering, `guix system image -t docker` /
//! `(gnu system image)`. The package build path is already Guile-free (the
//! build-recipe rail); this module is the foundation for constructing the OCI/disk
//! IMAGE in Rust instead.
//!
//! Output format — docker-archive v1.x (`docker save` layout, cracked from a real
//! `guix system image -t docker`): skopeo reads it via `docker-archive:PATH` and crun
//! runs it. guix gzips the tar, but skopeo also accepts an UNCOMPRESSED tar — and
//! td-builder is zero-dep (no gzip crate) — so we emit an uncompressed `.tar`:
//!
//!   manifest.json   [{"Config":"config.json","RepoTags":["td:latest"],
//!                     "Layers":["<id>/layer.tar"]}]
//!   config.json     image config; rootfs.diff_ids[0] = "sha256:"+sha256(layer.tar)
//!   <id>/VERSION    "1.0"
//!   <id>/json       {"id":"<id>","created":"1970-01-01T00:00:01Z","container_config":null}
//!   <id>/layer.tar  the rootfs as a tar
//!   repositories    {"td":{"latest":"<id>"}}
//!
//! Determinism (prime directive 1): every entry is emitted in a fixed order with
//! normalized metadata (mtime=1 = SOURCE_DATE_EPOCH, uid/gid=0, empty uname/gname),
//! so the same rootfs always packs to byte-identical bytes. NO guix/Guile is involved.
//!
//! Scope (brick 1): pack a PREPARED rootfs directory into the archive. Laying a store
//! CLOSURE into the rootfs (brick 2) and the load/run/oracle gate (brick 3) come next.
//! Per-file sizes use ustar octal (the GNU base-256 large-size encoding for a
//! >8 GiB layer.tar lands in brick 2, with real OS closures).

use crate::sha256::Sha256;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const BLOCK: usize = 512;
/// SOURCE_DATE_EPOCH=1 — the same mtime guix normalizes image entries to.
const NORMAL_MTIME: u64 = 1;

/// The `config` object of the OCI/docker `config.json`.
pub struct ImageConfig {
    /// `REPO:TAG`, e.g. "td:latest".
    pub repo_tag: String,
    pub env: Vec<String>,
    pub entrypoint: Vec<String>,
    pub cmd: Vec<String>,
}

fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize()
}

// ---- deterministic ustar writer -------------------------------------------------

/// Write VALUE as a zero-padded, NUL-terminated octal field filling `buf` (the ustar
/// numeric convention: `buf.len()-1` octal digits then a NUL).
fn octal_field(buf: &mut [u8], value: u64) {
    let w = buf.len() - 1;
    let s = format!("{value:0w$o}");
    let b = s.as_bytes();
    // A value that overflows the field is a brick-2 concern (base-256); refuse loudly.
    let start = b.len().saturating_sub(w);
    let digits = &b[start..];
    buf[..digits.len()].copy_from_slice(digits);
    buf[digits.len()] = 0;
}

fn pad_to_block(out: &mut Vec<u8>) {
    let rem = out.len() % BLOCK;
    if rem != 0 {
        out.resize(out.len() + (BLOCK - rem), 0);
    }
}

/// Emit one ustar header block for NAME (already <=100 bytes) into `out`.
fn write_header(out: &mut Vec<u8>, name: &str, typeflag: u8, mode: u32, size: u64, link: &str) {
    let mut h = [0u8; BLOCK];
    let nb = name.as_bytes();
    let nlen = nb.len().min(100);
    h[..nlen].copy_from_slice(&nb[..nlen]);
    octal_field(&mut h[100..108], (mode & 0o7777) as u64);
    octal_field(&mut h[108..116], 0); // uid
    octal_field(&mut h[116..124], 0); // gid
    octal_field(&mut h[124..136], size);
    octal_field(&mut h[136..148], NORMAL_MTIME);
    h[156] = typeflag;
    let lb = link.as_bytes();
    let llen = lb.len().min(100);
    h[157..157 + llen].copy_from_slice(&lb[..llen]);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    // Checksum: sum of all header bytes with the checksum field taken as spaces.
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let cs = format!("{sum:06o}");
    h[148..148 + cs.len()].copy_from_slice(cs.as_bytes());
    h[154] = 0;
    h[155] = b' ';
    out.extend_from_slice(&h);
}

/// Append one tar entry (header + body, each block-padded), emitting a GNU '@LongLink'
/// pseudo-entry first when NAME exceeds the 100-byte ustar name field (store paths do).
fn tar_entry(out: &mut Vec<u8>, name: &str, typeflag: u8, mode: u32, body: &[u8], link: &str) {
    if name.len() > 100 {
        let mut nbytes = name.as_bytes().to_vec();
        nbytes.push(0);
        write_header(out, "././@LongLink", b'L', 0o644, nbytes.len() as u64, "");
        out.extend_from_slice(&nbytes);
        pad_to_block(out);
    }
    write_header(out, name, typeflag, mode, body.len() as u64, link);
    if !body.is_empty() {
        out.extend_from_slice(body);
        pad_to_block(out);
    }
}

/// Serialize the rootfs at ROOT as a deterministic tar (no trailing zero blocks —
/// `build_layer_tar` adds the EOF marker). Entry names are relative to ROOT, sorted,
/// directories carry a trailing "/" (tar convention), with normalized metadata.
fn append_tree(out: &mut Vec<u8>, root: &Path) -> io::Result<()> {
    fn walk(out: &mut Vec<u8>, base: &Path, rel: &str) -> io::Result<()> {
        let path = if rel.is_empty() { base.to_path_buf() } else { base.join(rel) };
        let meta = fs::symlink_metadata(&path)?;
        let ft = meta.file_type();
        if ft.is_dir() {
            if !rel.is_empty() {
                tar_entry(out, &format!("{rel}/"), b'5', 0o755, &[], "");
            }
            let mut names: Vec<String> = fs::read_dir(&path)?
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<io::Result<_>>()?;
            names.sort();
            for n in names {
                let child = if rel.is_empty() { n } else { format!("{rel}/{n}") };
                walk(out, base, &child)?;
            }
        } else if ft.is_symlink() {
            let target = fs::read_link(&path)?;
            tar_entry(out, rel, b'2', 0o777, &[], &target.to_string_lossy());
        } else if ft.is_file() {
            let body = fs::read(&path)?;
            let mode = if meta.permissions().mode() & 0o100 != 0 { 0o755 } else { 0o644 };
            tar_entry(out, rel, b'0', mode, &body, "");
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: unsupported file type for an OCI layer", path.display()),
            ));
        }
        Ok(())
    }
    walk(out, root, "")
}

/// The rootfs at ROOT serialized as a complete layer tar (with the two-zero-block EOF).
pub fn build_layer_tar(root: &Path) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    append_tree(&mut out, root)?;
    out.resize(out.len() + BLOCK * 2, 0);
    Ok(out)
}

// ---- JSON (hand-rolled, deterministic key order) --------------------------------

fn json_quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn json_str_array(items: &[String]) -> String {
    let parts: Vec<String> = items.iter().map(|s| json_quote(s)).collect();
    format!("[{}]", parts.join(","))
}

fn build_config_json(diff_hex: &str, cfg: &ImageConfig) -> String {
    let mut config_obj = format!(
        "\"env\":{},\"entrypoint\":{}",
        json_str_array(&cfg.env),
        json_str_array(&cfg.entrypoint)
    );
    if !cfg.cmd.is_empty() {
        config_obj.push_str(&format!(",\"cmd\":{}", json_str_array(&cfg.cmd)));
    }
    format!(
        "{{\"architecture\":\"amd64\",\"comment\":\"Generated by td-builder\",\
         \"created\":\"1970-01-01T00:00:01Z\",\"config\":{{{config_obj}}},\
         \"container_config\":null,\
         \"history\":[{{\"created\":\"1970-01-01T00:00:01Z\",\
         \"created_by\":\"td-builder oci-image\",\"comment\":\"td-native\"}}],\
         \"os\":\"linux\",\"rootfs\":{{\"type\":\"layers\",\
         \"diff_ids\":[\"sha256:{diff_hex}\"]}}}}"
    )
}

fn split_repo_tag(repo_tag: &str) -> (&str, &str) {
    match repo_tag.rsplit_once(':') {
        Some((r, t)) => (r, t),
        None => (repo_tag, "latest"),
    }
}

/// Write a complete uncompressed docker-archive of the rootfs at LAYER_ROOT, with the
/// given image CONFIG, to OUT. No guix/Guile; deterministic.
pub fn write_docker_archive(
    out: &mut impl Write,
    layer_root: &Path,
    cfg: &ImageConfig,
) -> io::Result<()> {
    let layer = build_layer_tar(layer_root)?;
    let diff_hex = crate::sha256::to_base16(&sha256_bytes(&layer));
    // A deterministic v1 layer-dir id. skopeo recomputes digests from the bytes, so
    // any stable unique label works; derive it from the layer hash (distinct from it).
    let id = crate::sha256::to_base16(&sha256_bytes(format!("td-layer:{diff_hex}").as_bytes()));
    let config_json = build_config_json(&diff_hex, cfg);
    let manifest_json = format!(
        "[{{\"Config\":\"config.json\",\"RepoTags\":[{}],\"Layers\":[\"{id}/layer.tar\"]}}]",
        json_quote(&cfg.repo_tag)
    );
    let layer_json = format!(
        "{{\"id\":\"{id}\",\"created\":\"1970-01-01T00:00:01Z\",\"container_config\":null}}"
    );
    let (repo, tag) = split_repo_tag(&cfg.repo_tag);
    let repositories = format!("{{{}:{{{}:{}}}}}", json_quote(repo), json_quote(tag), json_quote(&id));

    let mut buf = Vec::new();
    tar_entry(&mut buf, &format!("{id}/"), b'5', 0o755, &[], "");
    tar_entry(&mut buf, &format!("{id}/VERSION"), b'0', 0o644, b"1.0", "");
    tar_entry(&mut buf, &format!("{id}/json"), b'0', 0o644, layer_json.as_bytes(), "");
    tar_entry(&mut buf, &format!("{id}/layer.tar"), b'0', 0o644, &layer, "");
    tar_entry(&mut buf, "config.json", b'0', 0o644, config_json.as_bytes(), "");
    tar_entry(&mut buf, "manifest.json", b'0', 0o644, manifest_json.as_bytes(), "");
    tar_entry(&mut buf, "repositories", b'0', 0o644, repositories.as_bytes(), "");
    buf.resize(buf.len() + BLOCK * 2, 0);
    out.write_all(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// Minimal ustar reader for the tests: walk 512-blocks, honour GNU '@LongLink',
    /// return (name, typeflag, body) per entry until the zero-block EOF. Doubles as a
    /// validator that headers are block-aligned and checksums are well-formed.
    fn read_tar(archive: &[u8]) -> Vec<(String, u8, Vec<u8>)> {
        let mut entries = Vec::new();
        let mut pos = 0usize;
        let mut pending_long: Option<String> = None;
        while pos + BLOCK <= archive.len() {
            let h = &archive[pos..pos + BLOCK];
            if h.iter().all(|&b| b == 0) {
                break;
            }
            // verify checksum
            let stored: u32 = {
                let f = &h[148..156];
                let s: String = f.iter().take_while(|&&b| b != 0 && b != b' ')
                    .map(|&b| b as char).collect();
                u32::from_str_radix(s.trim(), 8).unwrap()
            };
            let mut hh = [0u8; BLOCK];
            hh.copy_from_slice(h);
            hh[148..156].fill(b' ');
            let sum: u32 = hh.iter().map(|&b| b as u32).sum();
            assert_eq!(sum, stored, "tar header checksum mismatch at block {}", pos / BLOCK);
            let name_field: String = h[..100].iter().take_while(|&&b| b != 0)
                .map(|&b| b as char).collect();
            let typeflag = h[156];
            let size = {
                let s: String = h[124..136].iter().take_while(|&&b| b != 0 && b != b' ')
                    .map(|&b| b as char).collect();
                u64::from_str_radix(s.trim(), 8).unwrap_or(0) as usize
            };
            pos += BLOCK;
            let body = archive[pos..pos + size].to_vec();
            pos += size.div_ceil(BLOCK) * BLOCK;
            if typeflag == b'L' {
                let mut s = String::from_utf8(body).unwrap();
                if s.ends_with('\0') { s.pop(); }
                pending_long = Some(s);
                continue;
            }
            let name = pending_long.take().unwrap_or(name_field);
            entries.push((name, typeflag, body));
        }
        entries
    }

    fn sample_cfg() -> ImageConfig {
        ImageConfig {
            repo_tag: "td:latest".into(),
            env: vec!["PATH=/run/current-system/profile/bin".into()],
            entrypoint: vec!["/gnu/store/abc-boot".into(), "/gnu/store/def-system".into()],
            cmd: vec![],
        }
    }

    fn make_rootfs(dir: &Path) {
        fs::create_dir_all(dir.join("gnu/store/pkg/bin")).unwrap();
        fs::write(dir.join("gnu/store/pkg/bin/hello"), b"#!/bin/sh\necho hi\n").unwrap();
        let mut p = fs::metadata(dir.join("gnu/store/pkg/bin/hello")).unwrap().permissions();
        p.set_mode(0o755);
        fs::set_permissions(dir.join("gnu/store/pkg/bin/hello"), p).unwrap();
        fs::write(dir.join("gnu/store/pkg/readme"), b"hello\n").unwrap();
        fs::create_dir_all(dir.join("var/guix/gcroots")).unwrap();
        symlink("/gnu/store/pkg", dir.join("var/guix/gcroots/booted-system")).unwrap();
    }

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("td-oci-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn archive_has_expected_members() {
        let d = tmpdir("members");
        let rootfs = d.join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        make_rootfs(&rootfs);
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        let names: Vec<String> = read_tar(&out).into_iter().map(|(n, _, _)| n).collect();
        assert!(names.iter().any(|n| n == "manifest.json"), "no manifest.json: {names:?}");
        assert!(names.iter().any(|n| n == "config.json"), "no config.json");
        assert!(names.iter().any(|n| n == "repositories"), "no repositories");
        assert!(names.iter().any(|n| n.ends_with("/layer.tar")), "no layer.tar");
        assert!(names.iter().any(|n| n.ends_with("/VERSION")), "no VERSION");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn diff_id_equals_sha256_of_layer_tar() {
        let d = tmpdir("diffid");
        let rootfs = d.join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        make_rootfs(&rootfs);
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        let entries = read_tar(&out);
        let layer = &entries.iter().find(|(n, _, _)| n.ends_with("/layer.tar")).unwrap().2;
        let config = &entries.iter().find(|(n, _, _)| n == "config.json").unwrap().2;
        let want = format!("sha256:{}", crate::sha256::to_base16(&sha256_bytes(layer)));
        let config_str = String::from_utf8(config.clone()).unwrap();
        assert!(
            config_str.contains(&want),
            "config.json diff_id does not match sha256(layer.tar)\n want {want}\n config {config_str}"
        );
        // The layer.tar must itself be a well-formed tar (re-read it).
        let inner = read_tar(layer);
        assert!(inner.iter().any(|(n, _, _)| n == "gnu/store/pkg/bin/hello"));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn deterministic_byte_identical() {
        let d = tmpdir("determ");
        let r1 = d.join("a");
        let r2 = d.join("b");
        fs::create_dir_all(&r1).unwrap();
        fs::create_dir_all(&r2).unwrap();
        make_rootfs(&r1);
        make_rootfs(&r2);
        let mut o1 = Vec::new();
        let mut o2 = Vec::new();
        write_docker_archive(&mut o1, &r1, &sample_cfg()).unwrap();
        write_docker_archive(&mut o2, &r2, &sample_cfg()).unwrap();
        assert_eq!(o1, o2, "same rootfs+config must pack byte-identically");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn long_store_path_roundtrips_via_longlink() {
        let d = tmpdir("longname");
        let rootfs = d.join("rootfs");
        // A store-path-length name (>100 bytes) to exercise the GNU @LongLink path.
        let long = format!("gnu/store/{}-some-very-long-package-name-2.0", "a".repeat(80));
        assert!(long.len() > 100);
        let full = rootfs.join(&long);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(&full, b"x").unwrap();
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        // The layer.tar inside must carry the full long name (recovered via @LongLink).
        let entries = read_tar(&out);
        let layer = &entries.iter().find(|(n, _, _)| n.ends_with("/layer.tar")).unwrap().2;
        let inner: Vec<String> = read_tar(layer).into_iter().map(|(n, _, _)| n).collect();
        assert!(inner.iter().any(|n| n == &long), "long name not recovered: {inner:?}");
        fs::remove_dir_all(&d).unwrap();
    }
}
