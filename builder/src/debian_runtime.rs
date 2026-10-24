//! Import the reviewed runtime subset of the pinned Debian build root.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

const GLIBC_FILES: &str = include_str!("debian-glibc-files.txt");
const GCC_FILES: &[(&str, &str)] = &[
    ("usr/lib/gcc/x86_64-linux-gnu/14/libgcc.a", "lib/libgcc.a"),
    (
        "usr/lib/gcc/x86_64-linux-gnu/14/libgcc_eh.a",
        "lib/libgcc_eh.a",
    ),
    (
        "usr/lib/gcc/x86_64-linux-gnu/14/crtbegin.o",
        "lib/crtbegin.o",
    ),
    (
        "usr/lib/gcc/x86_64-linux-gnu/14/crtbeginS.o",
        "lib/crtbeginS.o",
    ),
    (
        "usr/lib/gcc/x86_64-linux-gnu/14/crtbeginT.o",
        "lib/crtbeginT.o",
    ),
    ("usr/lib/gcc/x86_64-linux-gnu/14/crtend.o", "lib/crtend.o"),
    ("usr/lib/gcc/x86_64-linux-gnu/14/crtendS.o", "lib/crtendS.o"),
    (
        "usr/lib/x86_64-linux-gnu/libgcc_s.so.1",
        "lib/libgcc_s.so.1",
    ),
];

pub(crate) fn import(root: &Path, out: &Path, kind: &str) -> Result<(), String> {
    match kind {
        "glibc-v2" => {
            let stage = out.join(td_engine::rust_dist::GLIBC_X86_64_STAGE);
            let loader = stage.join("lib/ld-linux-x86-64.so.2");
            let loader = loader
                .to_str()
                .ok_or("Debian runtime loader path is not UTF-8")?;
            for line in GLIBC_FILES.lines().filter(|line| !line.starts_with('#')) {
                let (source, destination) = line
                    .split_once('\t')
                    .ok_or("invalid Debian glibc file roster")?;
                copy(root, &stage, source, destination)?;
                let file = stage.join(destination);
                if crate::elf::is_runtime_elf(&file)?
                    && td_engine::elf::read_interp(&file)?.is_some()
                {
                    td_engine::elf_edit::set_interp(&file, loader)?;
                }
            }
            fs::write(
                stage.join("lib/libc.so"),
                "/* GNU ld script */\nGROUP ( libc.so.6 libc_nonshared.a AS_NEEDED ( ld-linux-x86-64.so.2 ) )\n",
            )
            .map_err(|e| format!("write libc linker script: {e}"))?;
            fs::write(
                stage.join("lib/libm.so"),
                "/* GNU ld script */\nGROUP ( libm.so.6 AS_NEEDED ( libmvec.so.1 ) )\n",
            )
            .map_err(|e| format!("write libm linker script: {e}"))?;
            fs::write(
                stage.join("lib/libm.a"),
                "/* GNU ld script */\nGROUP ( libm-2.41.a libmvec.a )\n",
            )
            .map_err(|e| format!("write static libm linker script: {e}"))?;
        }
        "gcc-v1" => {
            for &(source, destination) in GCC_FILES {
                copy(root, out, source, destination)?;
            }
            fs::write(
                out.join("lib/libgcc_s.so"),
                "GROUP ( libgcc_s.so.1 -lgcc )\n",
            )
            .map_err(|e| format!("write libgcc linker script: {e}"))?;
        }
        _ => return Err(format!("unknown Debian runtime import {kind:?}")),
    }
    fs::write(out.join("debian-runtime"), format!("{kind}\n"))
        .map_err(|e| format!("write Debian runtime provenance: {e}"))
}

fn copy(root: &Path, out: &Path, source: &str, destination: &str) -> Result<(), String> {
    for path in [source, destination] {
        if Path::new(path)
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(format!("nonrelative Debian runtime path {path:?}"));
        }
    }
    let source = root.join(source);
    let metadata =
        fs::symlink_metadata(&source).map_err(|e| format!("inspect {}: {e}", source.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "Debian runtime source is not a regular file: {}",
            source.display()
        ));
    }
    let destination = out.join(destination);
    let parent = destination
        .parent()
        .ok_or("Debian runtime destination has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    fs::copy(&source, &destination).map_err(|e| format!("copy {}: {e}", source.display()))?;
    fs::set_permissions(
        &destination,
        fs::Permissions::from_mode(0o644 | (metadata.permissions().mode() & 0o111)),
    )
    .map_err(|e| format!("chmod {}: {e}", destination.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_temp::TempDir;
    use std::collections::BTreeSet;

    #[test]
    fn reviewed_glibc_roster_is_unique_and_confined() {
        let mut destinations = BTreeSet::new();
        for line in GLIBC_FILES.lines().filter(|line| !line.starts_with('#')) {
            let (source, destination) = line.split_once('\t').unwrap();
            assert!(source.starts_with("usr/"));
            assert!(["include/", "lib/", "bin/", "sbin/"]
                .iter()
                .any(|prefix| destination.starts_with(prefix)));
            for path in [source, destination] {
                assert!(Path::new(path)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_))));
            }
            assert!(destinations.insert(destination), "duplicate {destination}");
        }
        for needed in [
            "lib/libc.so.6",
            "lib/ld-linux-x86-64.so.2",
            "lib/libc.a",
            "lib/Scrt1.o",
            "include/stdio.h",
            "include/asm/unistd.h",
            "sbin/zic",
            "bin/zdump",
        ] {
            assert!(destinations.contains(needed), "missing {needed}");
        }
    }

    #[test]
    fn runtime_copy_refuses_symlinks_and_clears_special_modes() {
        let temp = TempDir::new("debian-runtime-copy").unwrap();
        let root = temp.path().join("root");
        let out = temp.path().join("out");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("object"), b"pinned object").unwrap();
        fs::set_permissions(root.join("object"), fs::Permissions::from_mode(0o4755)).unwrap();
        copy(&root, &out, "object", "lib/object").unwrap();
        assert_eq!(fs::read(out.join("lib/object")).unwrap(), b"pinned object");
        assert_eq!(
            fs::metadata(out.join("lib/object"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
        std::os::unix::fs::symlink("object", root.join("link")).unwrap();
        assert!(copy(&root, &out, "link", "lib/link").is_err());
        assert!(copy(&root, &out, "object", "../escaped").is_err());
        assert!(import(&root, &out, "unknown").is_err());
    }
}
