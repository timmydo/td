//! PostgreSQL runtimes that can be reproduced inside the isolated restore jail.
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use td_engine::allocation;

const UNSUPPORTED: &str = "unsupported PostgreSQL restore package: use a Guix PostgreSQL 17 store package, /usr/lib/postgresql/17/bin, /opt/td-mta/postgresql/usr/lib/postgresql/17/bin, /usr/pgsql-17/bin or /usr/local/pgsql/bin";
const EXTRACTED: &str = "/opt/td-mta/postgresql/usr";
const SYSTEM_LIBS: &[&str] = &[
    "/lib/x86_64-linux-gnu",
    "/usr/lib/x86_64-linux-gnu",
    "/lib/aarch64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/lib64",
    "/usr/lib64",
];
const DRILL_TOOLS: &[&str] = &["initdb", "postgres", "psql", "pg_restore"];

pub(crate) struct Layout {
    pub bin: PathBuf,
    pub lib: PathBuf,
    pub share: PathBuf,
    pub extra_libs: Vec<PathBuf>,
    pub library_path: Option<String>,
}

fn diagnostic(message: &str, path: &Path) -> String {
    allocation::format(format_args!("{message}: {}", path.display())).unwrap_or_default()
}

fn subpath(root: &Path, child: &str) -> Result<PathBuf, String> {
    Ok(PathBuf::from(
        allocation::format(format_args!(
            "{}/{child}",
            root.to_str().ok_or("invalid runtime path")?
        ))
        .map_err(|_| String::new())?,
    ))
}

fn package_dirs(bin: &Path) -> Result<(PathBuf, PathBuf), String> {
    let root = bin.parent().ok_or(UNSUPPORTED)?;
    let share = match bin.to_str() {
        Some("/usr/lib/postgresql/17/bin") => PathBuf::from("/usr/share/postgresql/17"),
        Some("/opt/td-mta/postgresql/usr/lib/postgresql/17/bin") => {
            subpath(Path::new(EXTRACTED), "share/postgresql/17")?
        }
        Some("/usr/local/pgsql/bin" | "/usr/pgsql-17/bin") => subpath(root, "share")?,
        _ if root.parent() == Some(Path::new("/gnu/store")) && bin.ends_with("bin") => {
            subpath(root, "share")?
        }
        _ => return Err(diagnostic(UNSUPPORTED, bin)),
    };
    Ok((subpath(root, "lib")?, share))
}

fn multiarch(name: &OsStr) -> bool {
    name == "x86_64-linux-gnu" || name == "aarch64-linux-gnu"
}

fn immutable_guix_lib(path: &Path) -> bool {
    let Ok(rest) = path.strip_prefix("/gnu/store") else {
        return false;
    };
    let mut parts = rest.iter();
    parts.next().is_some() && parts.next() == Some(OsStr::new("lib"))
}

// A host-linked controller may find libgcc through a Guix profile RUNPATH.
// Inside the jail the same file is available at its immutable store path.
fn mapped_compiler_library(maps: &[u8]) -> Option<&str> {
    for line in maps.split(|b| *b == b'\n') {
        let Some(path) = std::str::from_utf8(line)
            .ok()
            .and_then(|line| line.split_ascii_whitespace().nth(5))
            .map(Path::new)
        else {
            continue;
        };
        if path.file_name() == Some(OsStr::new("libgcc_s.so.1")) {
            if let Some(dir) = path.parent().filter(|dir| immutable_guix_lib(dir)) {
                return dir.to_str();
            }
        }
    }
    None
}

pub(super) fn controller_library_path(
    postgres: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut result = allocation::string(postgres.unwrap_or_default())?;
    if Path::new("/gnu/store").is_dir() {
        let mut buffer = [0u8; 64 * 1024];
        let mut out = &mut buffer[..];
        let used = usize::try_from(std::io::copy(
            &mut std::fs::File::open("/proc/self/maps")?,
            &mut out,
        )?)?;
        if let Some(dir) = buffer.get(..used).and_then(mapped_compiler_library) {
            if !result.split(':').any(|entry| entry == dir) {
                if !result.is_empty() {
                    allocation::push_str(&mut result, ":")?;
                }
                allocation::push_str(&mut result, dir)?;
            }
        }
    }
    Ok(result)
}

fn supported_lib(path: &Path, package_lib: &Path, bin: &Path) -> bool {
    path == package_lib
        || immutable_guix_lib(path)
        || SYSTEM_LIBS.iter().any(|dir| path == Path::new(dir))
        || (bin.starts_with(EXTRACTED)
            && path.parent() == Some(Path::new("/opt/td-mta/postgresql/usr/lib"))
            && path.file_name().is_some_and(multiarch))
}

pub(crate) fn version_17(name: &str, banner: &str) -> bool {
    let mut fields = banner.split_whitespace();
    fields.next() == Some(name)
        && fields.next() == Some("(PostgreSQL)")
        && fields
            .next()
            .and_then(|v| v.strip_prefix("17."))
            .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
}

// Starting at the executable resolves Guix profiles, whose bin directory
// contains links to executables in multiple independent store packages.
pub(crate) fn from_path(path: &str, libraries: Option<&OsStr>) -> Result<Layout, String> {
    let initdb = crate::stage0::find_in_path(path, "initdb")
        .ok_or("PostgreSQL 17 initdb is missing from PATH")?;
    let bin = initdb
        .parent()
        .filter(|dir| dir.is_absolute())
        .ok_or("expected an absolute PostgreSQL tool directory")?;
    qualify(bin, libraries)
}

pub(crate) fn qualify(bin: &Path, libraries: Option<&OsStr>) -> Result<Layout, String> {
    let initdb = subpath(bin, "initdb")?;
    let initdb = initdb
        .canonicalize()
        .map_err(|_| diagnostic("cannot locate PostgreSQL 17 initdb", &initdb))?;
    let bin = initdb
        .parent()
        .filter(|_| initdb.ends_with("initdb"))
        .ok_or(UNSUPPORTED)?;
    let (lib, share) = package_dirs(bin)?;
    for path in [&lib, &share] {
        if !path.is_dir() || path.canonicalize().ok().as_deref() != Some(path.as_path()) {
            return Err(diagnostic(
                "missing or redirected PostgreSQL runtime directory",
                path,
            ));
        }
    }
    if !subpath(&share, "postgres.bki")?.is_file() {
        return Err(diagnostic(
            "missing PostgreSQL bootstrap catalog in",
            &share,
        ));
    }
    let mut extra_libs = Vec::new();
    let mut library_path = String::new();
    if let Some(value) = libraries {
        for part in std::env::split_paths(value) {
            if !part.is_absolute() || extra_libs.len() >= 16 {
                return Err("expected at most 16 absolute PostgreSQL library directories".into());
            }
            let resolved = part
                .canonicalize()
                .map_err(|_| diagnostic("cannot locate PostgreSQL library directory", &part))?;
            if !resolved.is_dir() || !supported_lib(&resolved, &lib, bin) {
                return Err(diagnostic(
                    "unsupported PostgreSQL library directory",
                    &part,
                ));
            }
            if !library_path.is_empty() {
                allocation::push_str(&mut library_path, ":").map_err(|_| String::new())?;
            }
            allocation::push_str(
                &mut library_path,
                resolved.to_str().ok_or("invalid library path")?,
            )
            .map_err(|_| String::new())?;
            allocation::push(&mut extra_libs, resolved).map_err(|_| String::new())?;
        }
    }
    for &name in DRILL_TOOLS {
        let path = subpath(bin, name)?;
        if !crate::stage0::is_exec(&path)
            || path.canonicalize().ok().as_deref() != Some(path.as_path())
        {
            return Err(diagnostic(
                "missing or redirected PostgreSQL 17 restore executable",
                &path,
            ));
        }
        let mut tool = Command::new(&path);
        tool.env_clear()
            .env("LC_ALL", "C")
            .arg("--version")
            .stdin(Stdio::null());
        if !library_path.is_empty() {
            tool.env("LD_LIBRARY_PATH", &library_path);
        }
        let output = tool
            .output()
            .map_err(|_| diagnostic("cannot execute PostgreSQL restore tool", &path))?;
        if !output.status.success()
            || !version_17(
                name,
                std::str::from_utf8(&output.stdout).unwrap_or_default(),
            )
        {
            return Err(diagnostic("restore tool must report PostgreSQL 17", &path));
        }
    }
    Ok(Layout {
        bin: bin.into(),
        lib,
        share,
        extra_libs,
        library_path: libraries.map(|_| library_path),
    })
}

// Host library mounts are separate from the PostgreSQL package directories.
// In particular /usr/lib can itself contain a whole PostgreSQL installation.
pub(crate) fn system_runtime() -> impl Iterator<Item = &'static Path> {
    SYSTEM_LIBS
        .iter()
        .copied()
        .chain(["/gnu/store", "/usr/share/zoneinfo", "/bin/sh"])
        .map(Path::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_uses_the_loaded_store_library_without_exposing_profile_paths() {
        let maps = b"0-100 r--p 0 0:0 1 /home/user/.profile/lib/libgcc_s.so.1\n100-200 r-xp 0 0:0 2 /gnu/store/pkg/lib/libgcc_s.so.1\n";
        assert_eq!(mapped_compiler_library(maps), Some("/gnu/store/pkg/lib"));
        assert_eq!(
            mapped_compiler_library(b"0-100 r--p 0 0:0 1 /gnu/store/pkg/etc/libgcc_s.so.1"),
            None
        );
    }

    #[test]
    fn resolves_the_actual_package_behind_a_profile_link() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = crate::test_temp::TempDir::new("restore-pg-profile").unwrap();
        let profile = root.join("profile");
        let package = root.join("foreign-package");
        std::fs::create_dir(&profile).unwrap();
        std::fs::create_dir(&package).unwrap();
        let init = package.join("initdb");
        std::fs::write(&init, b"fixture").unwrap();
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(init, profile.join("initdb")).unwrap();
        let err = from_path(profile.to_str().unwrap(), None).err().unwrap();
        assert!(err.ends_with(package.to_str().unwrap()));
        assert!(err.contains("unsupported PostgreSQL restore package"));
    }

    #[test]
    fn packages_bind_runtime_subdirectories_only() {
        for (bin, lib, share) in [
            (
                "/usr/lib/postgresql/17/bin",
                "/usr/lib/postgresql/17/lib",
                "/usr/share/postgresql/17",
            ),
            (
                "/opt/td-mta/postgresql/usr/lib/postgresql/17/bin",
                "/opt/td-mta/postgresql/usr/lib/postgresql/17/lib",
                "/opt/td-mta/postgresql/usr/share/postgresql/17",
            ),
            (
                "/usr/pgsql-17/bin",
                "/usr/pgsql-17/lib",
                "/usr/pgsql-17/share",
            ),
            (
                "/usr/local/pgsql/bin",
                "/usr/local/pgsql/lib",
                "/usr/local/pgsql/share",
            ),
            (
                "/gnu/store/package-postgresql-17/bin",
                "/gnu/store/package-postgresql-17/lib",
                "/gnu/store/package-postgresql-17/share",
            ),
        ] {
            let dirs = package_dirs(Path::new(bin)).unwrap();
            assert_eq!(
                (dirs.0.as_path(), dirs.1.as_path()),
                (Path::new(lib), Path::new(share))
            );
            for system in system_runtime() {
                if !bin.starts_with("/gnu/store/") {
                    assert!(!Path::new(bin).starts_with(system));
                }
            }
        }
        for bad in [
            "/home/operator/postgres/bin",
            "/usr/local/bin",
            "/usr/lib/postgresql/18/bin",
            "/usr/pgsql-17/data",
            "/gnu/store/pkg/etc/bin",
        ] {
            assert!(package_dirs(Path::new(bad))
                .unwrap_err()
                .contains("unsupported PostgreSQL restore package"));
        }
    }

    #[test]
    fn library_paths_cannot_expose_other_package_trees() {
        let bin = Path::new("/opt/td-mta/postgresql/usr/lib/postgresql/17/bin");
        let (lib, _) = package_dirs(bin).unwrap();
        for dir in [
            "/home/mail",
            "/usr/lib",
            "/usr/local/pgsql",
            "/opt/td-mta/postgresql/usr/lib",
            "/opt/td-mta/postgresql/usr/lib/data",
            "/gnu/store/pkg/etc",
        ] {
            assert!(!supported_lib(Path::new(dir), &lib, bin));
        }
        assert!(supported_lib(
            Path::new("/opt/td-mta/postgresql/usr/lib/x86_64-linux-gnu"),
            &lib,
            bin
        ));
    }
}
