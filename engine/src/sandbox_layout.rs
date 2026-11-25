//! Sandbox share destinations. The source is always a home-relative grant
//! name. Destinations describe only where to place an already admitted view.
use std::io;
use std::path::{Path, PathBuf};

pub const MAX_MOUNTS: usize = 32;
const PATH_BYTES: usize = 1024;
pub const MAX_BYTES: usize = MAX_MOUNTS * (2 * PATH_BYTES + 8);
pub const RESERVED: &[&str] = &[
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/gnu",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/nix",
    "/oldroot",
    "/opt",
    "/proc",
    "/root-write-probe",
    "/run",
    "/sbin",
    "/sys",
    "/td",
    "/tmp",
    "/usr",
    "/var/tmp",
    "/var/log/svc",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout(Vec<(String, String)>);

fn invalid() -> io::Error {
    io::Error::other(
        "invalid sandbox mount path (use a canonical path or a path relative to the sandbox home)",
    )
}

fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= PATH_BYTES
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && part.len() <= 255)
}

pub fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

pub fn destination(value: &str) -> io::Result<&str> {
    let value = value.strip_prefix("~/").unwrap_or(value);
    if let Some(path) = value.strip_prefix('/') {
        if !relative(path)
            || value.len() > PATH_BYTES
            || RESERVED
                .iter()
                .chain(["/var/lib/td"].iter())
                .any(|fixed| overlaps(Path::new(value), Path::new(fixed)))
        {
            return Err(invalid());
        }
    } else if !relative(value) || value == "~" || value.starts_with("~/") {
        return Err(invalid());
    }
    Ok(value)
}

impl Layout {
    pub fn mounts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(src, dst)| (src.as_str(), dst.as_str()))
    }
    pub fn get(&self, source: &str) -> Option<&str> {
        self.mounts()
            .find_map(|(src, dst)| (src == source).then_some(dst))
    }
    pub fn targets(
        &self,
        source: &str,
        home: &Path,
        human: &Path,
    ) -> io::Result<(PathBuf, Option<PathBuf>)> {
        if !relative(source) {
            return Err(invalid());
        }
        let join = |base: &Path, path: &str| {
            let joined = if path.starts_with('/') {
                crate::allocation::format(format_args!("{path}"))
            } else {
                crate::allocation::format(format_args!("{}/{path}", base.display()))
            };
            joined
                .map(PathBuf::from)
                .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))
        };
        let mapped = self.get(source);
        let target = join(home, mapped.unwrap_or(source))?;
        if home.starts_with(&target) {
            return Err(io::Error::other("a share cannot cover the private home"));
        }
        let original = if mapped.is_none() && human != home {
            Some(join(human, source)?)
        } else {
            None
        };
        Ok((target, original))
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) {
        self.0.retain(|(src, _)| keep(src));
    }
    pub fn add(&mut self, source: &str, target: &str) -> io::Result<()> {
        let target = destination(target)?;
        if !relative(source) || self.0.len() >= MAX_MOUNTS || self.get(source).is_some() {
            return Err(invalid());
        }
        let oom = |_| io::Error::from(io::ErrorKind::OutOfMemory);
        let row = (
            crate::allocation::string(source).map_err(oom)?,
            crate::allocation::string(target).map_err(oom)?,
        );
        crate::allocation::push(&mut self.0, row).map_err(oom)
    }
    pub fn row(&mut self, line: &str) -> io::Result<()> {
        let (source, target) = line.split_once('\t').ok_or_else(invalid)?;
        self.add(source, target)
    }
    pub fn encode(&self, prefix: &str) -> io::Result<String> {
        let mut out = String::new();
        for (source, target) in self.mounts() {
            for s in [prefix, source, "\t", target, "\n"] {
                crate::allocation::push_str(&mut out, s)
                    .map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            }
        }
        Ok(out)
    }
}
