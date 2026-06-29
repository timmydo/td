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
//! Determinism (prime directive 1): every entry is emitted in a fixed order (sorted
//! names) with normalized metadata — mtime=1 (SOURCE_DATE_EPOCH), uid/gid=0, empty
//! uname/gname, and a normalized file mode (base 0644/0755 by exec bit, PRESERVING
//! setuid/setgid/sticky) — so the same rootfs always packs to byte-identical bytes. NO
//! guix/Guile is involved. Sizes use ustar octal, with GNU base-256 for fields a value
//! overflows (a >=8 GiB layer.tar / rootfs file — real OS closures), and GNU `@LongLink`
//! ('L'/'K') for names and symlink targets over 100 bytes (store paths and their
//! absolute-path link targets). A non-UTF8 name/target is an error, not a lossy mangle.
//!
//! Scope (brick 1): pack a PREPARED rootfs directory into the archive. Laying a store
//! CLOSURE into the rootfs (brick 2) and the load/run/oracle gate (brick 3) come next.

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

/// Write VALUE into the ustar numeric field `buf`. Small values use the classic
/// zero-padded, NUL-terminated octal form (`buf.len()-1` digits then NUL). A value too
/// large for octal (a `layer.tar` or rootfs file >= 8 GiB in the 12-byte size field —
/// real OS closures hit this) uses the GNU **base-256** encoding: the field holds the
/// number big-endian across all bytes with the high bit of byte 0 set as the marker
/// (Go's `archive/tar` and GNU tar both read it). This is what makes large images
/// correct instead of silently truncated — the earlier octal-only version dropped the
/// high digits and desynced the tar stream.
fn numeric_field(buf: &mut [u8], value: u64) {
    let w = buf.len();
    // Octal fits when value < 8^(w-1) (w-1 digits + a NUL). w is 8 or 12 here.
    let octal_fits = (w - 1) >= 22 || (value as u128) < (1u128 << (3 * (w - 1)));
    if octal_fits {
        let s = format!("{value:0width$o}", width = w - 1);
        buf[..w - 1].copy_from_slice(s.as_bytes());
        buf[w - 1] = 0;
    } else {
        buf.fill(0);
        let mut v = value;
        for slot in buf.iter_mut().rev() {
            *slot = (v & 0xff) as u8;
            v >>= 8;
        }
        buf[0] |= 0x80; // base-256 marker
    }
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
    numeric_field(&mut h[100..108], (mode & 0o7777) as u64);
    numeric_field(&mut h[108..116], 0); // uid
    numeric_field(&mut h[116..124], 0); // gid
    numeric_field(&mut h[124..136], size);
    numeric_field(&mut h[136..148], NORMAL_MTIME);
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

/// Append one tar entry (header + body, each block-padded), emitting GNU '@LongLink'
/// pseudo-entries first when the NAME (>100 bytes — store paths) or the symlink LINK
/// target (>100 bytes — absolute store-path targets) exceed the 100-byte ustar fields.
/// Without the 'K' entry a long symlink target is silently truncated to a wrong path.
fn tar_entry(out: &mut Vec<u8>, name: &str, typeflag: u8, mode: u32, body: &[u8], link: &str) {
    if link.len() > 100 {
        let mut lbytes = link.as_bytes().to_vec();
        lbytes.push(0);
        write_header(out, "././@LongLink", b'K', 0o644, lbytes.len() as u64, "");
        out.extend_from_slice(&lbytes);
        pad_to_block(out);
    }
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

/// Append SRC's tree to OUT as deterministic tar entries, each named PREFIX joined with
/// the path relative to SRC. PREFIX="" packs SRC's CONTENTS at the tar root; a non-empty
/// PREFIX places SRC's tree under that tar path (e.g. "gnu/store/<base>"). Sorted names,
/// directories carry a trailing "/", metadata normalized (mode preserves suid/sgid/
/// sticky); a non-UTF8 name/target errors. No trailing EOF blocks.
fn append_subtree(out: &mut Vec<u8>, src: &Path, prefix: &str) -> io::Result<()> {
    fn walk(out: &mut Vec<u8>, abspath: &Path, tarname: &str) -> io::Result<()> {
        let meta = fs::symlink_metadata(abspath)?;
        let ft = meta.file_type();
        if ft.is_dir() {
            if !tarname.is_empty() {
                tar_entry(out, &format!("{tarname}/"), b'5', 0o755, &[], "");
            }
            let mut names: Vec<String> = fs::read_dir(abspath)?
                .map(|e| {
                    e?.file_name().into_string().map_err(|n| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("non-UTF8 filename in layer: {}", n.to_string_lossy()),
                        )
                    })
                })
                .collect::<io::Result<_>>()?;
            names.sort();
            for n in names {
                let child = if tarname.is_empty() { n.clone() } else { format!("{tarname}/{n}") };
                walk(out, &abspath.join(&n), &child)?;
            }
        } else if ft.is_symlink() {
            let target = fs::read_link(abspath)?;
            let target = target.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 symlink target in layer")
            })?;
            tar_entry(out, tarname, b'2', 0o777, &[], target);
        } else if ft.is_file() {
            let body = fs::read(abspath)?;
            // Normalize mode but PRESERVE setuid/setgid/sticky (0o7000); base 0644/0755
            // by any exec bit — a flat collapse would drop a suid binary's bit.
            let m = meta.permissions().mode();
            let mode = (m & 0o7000) | if m & 0o111 != 0 { 0o755 } else { 0o644 };
            tar_entry(out, tarname, b'0', mode, &body, "");
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: unsupported file type for an OCI layer", abspath.display()),
            ));
        }
        Ok(())
    }
    walk(out, src, prefix)
}

/// The rootfs at ROOT serialized as a complete layer tar (with the two-zero-block EOF).
pub fn build_layer_tar(root: &Path) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    append_subtree(&mut out, root, "")?;
    out.resize(out.len() + BLOCK * 2, 0);
    Ok(out)
}

/// A layer tar laying each store PATH's tree at its location under STORE_DIR (so
/// `/gnu/store/<base>` -> tar `gnu/store/<base>/…`), preceded by the parent dir entries
/// (`gnu/`, `gnu/store/`). PATHS is a store closure (sorted + deduped here). This is how
/// td packs a real package/system closure into an image — no guix process, no temp copy.
pub fn build_layer_tar_from_store_paths(
    store_dir: &Path,
    paths: &[String],
) -> io::Result<Vec<u8>> {
    let store_rel = store_dir
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF8 store dir"))?
        .trim_matches('/'); // tolerate a trailing slash (no doubled separators)
    let mut out = Vec::new();
    // Emit each parent component (gnu/, gnu/store/) once, before the closure entries.
    let mut acc = String::new();
    for comp in store_rel.split('/').filter(|c| !c.is_empty()) {
        acc = if acc.is_empty() { comp.to_string() } else { format!("{acc}/{comp}") };
        tar_entry(&mut out, &format!("{acc}/"), b'5', 0o755, &[], "");
    }
    let mut sorted: Vec<&String> = paths.iter().collect();
    sorted.sort();
    sorted.dedup();
    for p in sorted {
        let base = Path::new(p).file_name().and_then(|b| b.to_str()).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, format!("bad store path: {p}"))
        })?;
        append_subtree(&mut out, Path::new(p), &format!("{store_rel}/{base}"))?;
    }
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
    // PascalCase keys per the OCI/docker image-config spec (Env/Entrypoint/Cmd). Go
    // consumers tolerate lowercase via case-insensitive matching, but a strict/non-Go
    // consumer would drop the entrypoint+env entirely.
    let mut config_obj = format!(
        "\"Env\":{},\"Entrypoint\":{}",
        json_str_array(&cfg.env),
        json_str_array(&cfg.entrypoint)
    );
    if !cfg.cmd.is_empty() {
        config_obj.push_str(&format!(",\"Cmd\":{}", json_str_array(&cfg.cmd)));
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

/// Wrap a finished LAYER tar into the docker-archive (manifest/config/<id>/… +
/// repositories) and write it to OUT. Shared by the rootfs-dir and store-closure paths.
fn assemble_archive(out: &mut impl Write, layer: &[u8], cfg: &ImageConfig) -> io::Result<()> {
    let diff_hex = crate::sha256::to_base16(&sha256_bytes(layer));
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
    tar_entry(&mut buf, &format!("{id}/layer.tar"), b'0', 0o644, layer, "");
    tar_entry(&mut buf, "config.json", b'0', 0o644, config_json.as_bytes(), "");
    tar_entry(&mut buf, "manifest.json", b'0', 0o644, manifest_json.as_bytes(), "");
    tar_entry(&mut buf, "repositories", b'0', 0o644, repositories.as_bytes(), "");
    buf.resize(buf.len() + BLOCK * 2, 0);
    out.write_all(&buf)
}

/// Write a complete uncompressed docker-archive of the rootfs at LAYER_ROOT, with the
/// given image CONFIG, to OUT. No guix/Guile; deterministic.
pub fn write_docker_archive(
    out: &mut impl Write,
    layer_root: &Path,
    cfg: &ImageConfig,
) -> io::Result<()> {
    assemble_archive(out, &build_layer_tar(layer_root)?, cfg)
}

/// Write a docker-archive whose single layer is the store CLOSURE PATHS laid out under
/// STORE_DIR (`build_layer_tar_from_store_paths`). This is the td-native replacement for
/// `guix system image -t docker`: the caller computes the closure (e.g. `Db::closure`,
/// no guix process) and td packs it. Deterministic; no guix/Guile.
pub fn write_docker_archive_from_store_paths(
    out: &mut impl Write,
    store_dir: &Path,
    paths: &[String],
    cfg: &ImageConfig,
) -> io::Result<()> {
    assemble_archive(out, &build_layer_tar_from_store_paths(store_dir, paths)?, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Entry {
        name: String,
        typeflag: u8,
        mode: u32,
        link: String,
        body: Vec<u8>,
    }

    fn octal_at(field: &[u8]) -> u64 {
        let s: String = field.iter().take_while(|&&b| b != 0 && b != b' ').map(|&b| b as char).collect();
        u64::from_str_radix(s.trim(), 8).unwrap_or(0)
    }

    /// Minimal ustar reader for the tests: walk 512-blocks, decode base-256 sizes, honour
    /// GNU '@LongLink' ('L' name, 'K' linkname), and return one Entry per real member
    /// until the zero-block EOF. Doubles as a validator that headers are block-aligned
    /// and checksums well-formed.
    fn read_tar(archive: &[u8]) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut pos = 0usize;
        let mut pending_name: Option<String> = None;
        let mut pending_link: Option<String> = None;
        while pos + BLOCK <= archive.len() {
            let h = &archive[pos..pos + BLOCK];
            if h.iter().all(|&b| b == 0) {
                break;
            }
            let stored = octal_at(&h[148..156]) as u32;
            let mut hh = [0u8; BLOCK];
            hh.copy_from_slice(h);
            hh[148..156].fill(b' ');
            let sum: u32 = hh.iter().map(|&b| b as u32).sum();
            assert_eq!(sum, stored, "tar header checksum mismatch at block {}", pos / BLOCK);
            let name_field: String = h[..100].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
            let mode = octal_at(&h[100..108]) as u32;
            let typeflag = h[156];
            let link_field: String = h[157..257].iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
            // size: GNU base-256 when the high bit of byte 0 is set, else octal.
            let sz = &h[124..136];
            let size = if sz[0] & 0x80 != 0 {
                let mut v: u128 = (sz[0] & 0x7f) as u128;
                for &b in &sz[1..] {
                    v = (v << 8) | b as u128;
                }
                v as usize
            } else {
                octal_at(sz) as usize
            };
            pos += BLOCK;
            let body = archive[pos..pos + size].to_vec();
            pos += size.div_ceil(BLOCK) * BLOCK;
            let detok = |b: Vec<u8>| {
                let mut s = String::from_utf8(b).unwrap();
                if s.ends_with('\0') {
                    s.pop();
                }
                s
            };
            match typeflag {
                b'L' => {
                    pending_name = Some(detok(body));
                    continue;
                }
                b'K' => {
                    pending_link = Some(detok(body));
                    continue;
                }
                _ => {}
            }
            entries.push(Entry {
                name: pending_name.take().unwrap_or(name_field),
                typeflag,
                mode,
                link: pending_link.take().unwrap_or(link_field),
                body,
            });
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
        let names: Vec<String> = read_tar(&out).into_iter().map(|e| e.name).collect();
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
        let layer = &entries.iter().find(|e| e.name.ends_with("/layer.tar")).unwrap().body;
        let config = &entries.iter().find(|e| e.name == "config.json").unwrap().body;
        let want = format!("sha256:{}", crate::sha256::to_base16(&sha256_bytes(layer)));
        let config_str = String::from_utf8(config.clone()).unwrap();
        assert!(
            config_str.contains(&want),
            "config.json diff_id does not match sha256(layer.tar)\n want {want}\n config {config_str}"
        );
        // The layer.tar must itself be a well-formed tar (re-read it).
        let inner = read_tar(layer);
        assert!(inner.iter().any(|e| e.name == "gnu/store/pkg/bin/hello"));
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
        let layer = &entries.iter().find(|e| e.name.ends_with("/layer.tar")).unwrap().body;
        let inner: Vec<String> = read_tar(layer).into_iter().map(|e| e.name).collect();
        assert!(inner.iter().any(|n| n == &long), "long name not recovered: {inner:?}");
        fs::remove_dir_all(&d).unwrap();
    }

    // ---- coverage for the review fixes -----------------------------------------

    #[test]
    fn numeric_field_octal_and_base256() {
        // Small values: classic octal, NUL-terminated.
        let mut f = [0u8; 12];
        numeric_field(&mut f, 1);
        assert_eq!(&f[..12], b"00000000001\0");
        numeric_field(&mut f, 0o777);
        assert_eq!(&f[..12], b"00000000777\0");
        // >= 8 GiB overflows 11 octal digits → GNU base-256 (high bit of byte 0 set),
        // big-endian value. The earlier octal-only code silently truncated this.
        let big: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB
        numeric_field(&mut f, big);
        assert_eq!(f[0] & 0x80, 0x80, "base-256 marker not set for {big}");
        let mut v: u128 = (f[0] & 0x7f) as u128;
        for &b in &f[1..] {
            v = (v << 8) | b as u128;
        }
        assert_eq!(v as u64, big, "base-256 round-trip wrong");
    }

    #[test]
    fn long_symlink_target_roundtrips_via_k() {
        let d = tmpdir("longtarget");
        let rootfs = d.join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        // An absolute store-path target >100 bytes — without the 'K' longlink this is
        // silently truncated to a wrong path.
        let target = format!("/gnu/store/{}-pkg-1.0/bin/the-real-binary", "b".repeat(70));
        assert!(target.len() > 100);
        symlink(&target, rootfs.join("bin/x")).unwrap();
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        let entries = read_tar(&out);
        let layer = &entries.iter().find(|e| e.name.ends_with("/layer.tar")).unwrap().body;
        let inner = read_tar(layer);
        let link = inner.iter().find(|e| e.name == "bin/x").expect("symlink missing");
        assert_eq!(link.typeflag, b'2', "not a symlink entry");
        assert_eq!(link.link, target, "long symlink target was truncated");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn config_uses_pascalcase_keys() {
        let d = tmpdir("pascal");
        let rootfs = d.join("rootfs");
        fs::create_dir_all(&rootfs).unwrap();
        make_rootfs(&rootfs);
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        let entries = read_tar(&out);
        let config = String::from_utf8(
            entries.iter().find(|e| e.name == "config.json").unwrap().body.clone(),
        )
        .unwrap();
        // OCI/docker image-config spec: Env/Entrypoint (PascalCase), not env/entrypoint.
        assert!(config.contains("\"Env\":"), "config not PascalCase: {config}");
        assert!(config.contains("\"Entrypoint\":"), "config not PascalCase: {config}");
        assert!(!config.contains("\"env\":"), "config has lowercase env: {config}");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn closure_layer_lays_store_paths_at_their_location() {
        let d = tmpdir("closure");
        // A fake store with two "packages"; build_layer_tar_from_store_paths must place
        // each under <store-rel>/<base>/… and emit the parent dir entries.
        let store = d.join("gnu/store");
        fs::create_dir_all(store.join("aaa-pkg-a/bin")).unwrap();
        fs::write(store.join("aaa-pkg-a/bin/a"), b"A").unwrap();
        fs::create_dir_all(store.join("bbb-pkg-b")).unwrap();
        fs::write(store.join("bbb-pkg-b/readme"), b"B").unwrap();
        let paths = vec![
            store.join("bbb-pkg-b").to_string_lossy().into_owned(),
            store.join("aaa-pkg-a").to_string_lossy().into_owned(), // unsorted on purpose
        ];
        let layer = build_layer_tar_from_store_paths(&store, &paths).unwrap();
        let names: Vec<String> = read_tar(&layer).into_iter().map(|e| e.name).collect();
        let rel = store.to_string_lossy().trim_start_matches('/').to_string();
        assert!(names.iter().any(|n| n == &format!("{rel}/")), "no store dir entry: {names:?}");
        assert!(names.iter().any(|n| n == &format!("{rel}/aaa-pkg-a/bin/a")), "pkg-a file missing");
        assert!(names.iter().any(|n| n == &format!("{rel}/bbb-pkg-b/readme")), "pkg-b file missing");
        // Deterministic + sorted regardless of input order.
        let layer2 = build_layer_tar_from_store_paths(&store, &[paths[1].clone(), paths[0].clone()]).unwrap();
        assert_eq!(layer, layer2, "closure layer must be order-independent + deterministic");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn setuid_bit_preserved() {
        let d = tmpdir("suid");
        let rootfs = d.join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        let bin = rootfs.join("bin/su");
        fs::write(&bin, b"x").unwrap();
        let mut p = fs::metadata(&bin).unwrap().permissions();
        p.set_mode(0o4755); // setuid + rwxr-xr-x
        fs::set_permissions(&bin, p).unwrap();
        let mut out = Vec::new();
        write_docker_archive(&mut out, &rootfs, &sample_cfg()).unwrap();
        let entries = read_tar(&out);
        let layer = &entries.iter().find(|e| e.name.ends_with("/layer.tar")).unwrap().body;
        let inner = read_tar(layer);
        let su = inner.iter().find(|e| e.name == "bin/su").expect("bin/su missing");
        assert_eq!(su.mode & 0o4000, 0o4000, "setuid bit dropped (mode {:o})", su.mode);
        fs::remove_dir_all(&d).unwrap();
    }
}
