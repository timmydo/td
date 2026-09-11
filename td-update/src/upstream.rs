//! Optional, unsigned source-companion settings; never an installation authority.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const NAME: &str = "upstream";
const LIMIT: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub origin: String,
    pub branch: String,
}

impl Upstream {
    pub fn new(origin: &str, branch: &str) -> Result<Self, String> {
        // A public HTTPS origin needs no embedded credentials or remote helper.
        let remote = origin
            .strip_prefix("https://")
            .ok_or("source origin must be a credential-free HTTPS URL")?;
        let (authority, path) = remote
            .split_once('/')
            .ok_or("source origin must include a host and repository path")?;
        if authority.is_empty()
            || path.is_empty()
            || !authority
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:[]".contains(&b))
            || !origin
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"\\?#".contains(&b))
        {
            return Err(
                "source origin must be a credential-free HTTPS URL with a repository path".into(),
            );
        }
        if branch.is_empty()
            || branch.starts_with('-')
            || branch == "HEAD"
            || branch.contains("..")
            || branch.contains("@{")
            || !branch
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b))
            || branch.split('/').any(|part| {
                part.is_empty()
                    || part.starts_with('.')
                    || part.ends_with('.')
                    || part.ends_with(".lock")
            })
        {
            return Err("source branch must be a plain Git branch name".into());
        }
        let upstream = Self {
            origin: origin.into(),
            branch: branch.into(),
        };
        if upstream.encode().len() as u64 > LIMIT {
            return Err("source upstream settings exceed 4096 bytes".into());
        }
        Ok(upstream)
    }

    pub fn encode(&self) -> String {
        format!("td-source-upstream-v1\n{}\n{}\n", self.origin, self.branch)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(bytes).map_err(|_| "source upstream is not UTF-8")?;
        let mut lines = text.split('\n');
        if lines.next() != Some("td-source-upstream-v1") {
            return Err("unknown source upstream format".into());
        }
        let origin = lines.next().ok_or("missing source origin")?;
        let branch = lines.next().ok_or("missing source branch")?;
        if lines.next() != Some("") || lines.next().is_some() {
            return Err("source upstream must contain exactly three terminated lines".into());
        }
        Self::new(origin, branch)
    }
}

pub fn read(source: &Path) -> Result<Option<Upstream>, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800) // Linux O_NOFOLLOW | O_NONBLOCK.
        .open(source.join(NAME))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("open source upstream: {error}")),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect source upstream: {error}"))?;
    if !metadata.is_file() || metadata.len() > LIMIT {
        return Err("source upstream must be a regular file of at most 4096 bytes".into());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read source upstream: {error}"))?;
    if bytes.len() as u64 > LIMIT {
        return Err("source upstream grew beyond 4096 bytes".into());
    }
    Upstream::parse(&bytes).map(Some)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_without_extra_fields_or_credentials() {
        let value = Upstream::new("https://example.invalid/td.git", "releases/rolling").unwrap();
        assert_eq!(Upstream::parse(value.encode().as_bytes()).unwrap(), value);
        for origin in [
            "/srv/git/td.git",
            "file:///repo",
            "ext::command",
            "-x",
            "https://user:secret@example.invalid/repo",
            "https://example.invalid/",
            "https://example.invalid/repo?token=secret",
            "https://example.invalid/repo\nextra",
        ] {
            assert!(Upstream::new(origin, "main").is_err(), "{origin}");
        }
        for branch in [
            "", "HEAD", "-x", "a..b", "a@{b", "a.lock", "a//b", "/a", "a/", ".a", "a.", "a b",
            "a\nb", "a:b", "a*", "a\\b",
        ] {
            assert!(
                Upstream::new("https://example.invalid/td.git", branch).is_err(),
                "{branch}"
            );
        }
        for suffix in ["extra\n", "\n"] {
            assert!(Upstream::parse(format!("{}{suffix}", value.encode()).as_bytes()).is_err());
        }
        assert!(Upstream::parse(value.encode().trim_end().as_bytes()).is_err());
        assert!(Upstream::new(
            &format!("https://example.invalid/{}", "x".repeat(4096)),
            "main"
        )
        .is_err());
    }
}
