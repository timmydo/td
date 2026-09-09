//! Bounded literal-path completion. Filesystem work never runs on the UI thread.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

type Result<T> = std::result::Result<T, String>;
const PATH_BYTES: usize = 4096;
const ENTRIES: usize = 4096;
const NAME_BYTES: usize = 1024 * 1024;
const MATCHES: usize = 128;

pub(crate) struct Matches {
    paths: Vec<String>,
    selected: Option<usize>,
    skipped: usize,
}

impl Matches {
    pub(crate) fn prefix(&self) -> &str {
        let Some(first) = self.paths.first() else {
            return "";
        };
        let mut end = first.len();
        for path in self.paths.iter().skip(1) {
            end = end.min(
                first
                    .bytes()
                    .zip(path.bytes())
                    .take_while(|(a, b)| a == b)
                    .count(),
            );
        }
        while !first.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        first.get(..end).unwrap_or_default()
    }

    pub(crate) fn cycle(&mut self, backward: bool) -> Option<&str> {
        let count = self.paths.len();
        if count == 0 {
            return None;
        }
        let index = match self.selected {
            None if backward => count - 1,
            None => 0,
            Some(index) if backward => (index + count - 1) % count,
            Some(index) => (index + 1) % count,
        };
        self.selected = Some(index);
        self.paths.get(index).map(String::as_str)
    }

    pub(crate) fn descend(&self, text: &str) -> bool {
        self.paths.len() == 1 && text.ends_with('/')
    }

    fn page(&self) -> impl Iterator<Item = (usize, &str)> {
        let start = self.selected.unwrap_or(0) / 3 * 3;
        self.paths
            .iter()
            .enumerate()
            .skip(start)
            .take(3)
            .map(|(n, p)| (n, p.as_str()))
    }
}

#[derive(Default)]
pub(crate) enum State {
    #[default]
    Idle,
    Pending(u64),
    Ready(Matches),
    Failed(String),
}

impl State {
    pub(crate) fn notice(&self) -> String {
        match self {
            Self::Idle => "Tab: complete; Up/Down: choose matches".into(),
            Self::Pending(_) => "Reading directory for completion...".into(),
            Self::Failed(detail) => format!(
                "Completion: {}",
                detail.chars().take(100).collect::<String>()
            ),
            Self::Ready(matches) => {
                let mut text = format!(
                    "{} matches; {} non-UTF-8 names omitted",
                    matches.paths.len(),
                    matches.skipped
                );
                for (index, path) in matches.page() {
                    let leaf = Path::new(path)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or(path);
                    let name: String = leaf.chars().flat_map(char::escape_debug).take(60).collect();
                    text.push_str(&format!(
                        "\n{} {}{}",
                        if matches.selected == Some(index) {
                            ">"
                        } else {
                            " "
                        },
                        name,
                        if path.ends_with('/') { "/" } else { "" }
                    ));
                }
                text
            }
        }
    }

    pub(crate) fn fields(&self) -> String {
        match self {
            Self::Idle => "completion=idle".into(),
            Self::Pending(id) => format!("completion=pending\tcompletion-id={id}"),
            Self::Failed(detail) => format!(
                "completion=error\tcompletion-error={}",
                crate::control::hex(detail.chars().take(160).collect::<String>().as_bytes())
            ),
            Self::Ready(matches) => {
                let mut fields = format!("completion=ready\tcompletion-count={}\tcompletion-skipped={}\tcompletion-selected={}",
                    matches.paths.len(), matches.skipped, matches.selected.map_or_else(|| "-".into(), |i| i.to_string()));
                for (index, path) in matches.page() {
                    fields.push_str(&format!(
                        "\tcompletion-item={index},{}",
                        crate::control::hex(path.as_bytes())
                    ));
                }
                fields
            }
        }
    }
}

pub(crate) struct Worker {
    requests: SyncSender<(u64, String)>,
    replies: Receiver<(u64, Result<Matches>)>,
    pending: Option<u64>,
}

impl Worker {
    pub(crate) fn start() -> Result<Self> {
        let (requests, input) = mpsc::sync_channel::<(u64, String)>(1);
        let (output, replies) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("editor-path-completion".into())
            .spawn(move || {
                while let Ok((id, text)) = input.recv() {
                    if output.send((id, scan(&text))).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(Self {
            requests,
            replies,
            pending: None,
        })
    }

    pub(crate) fn request(&mut self, id: u64, text: String) -> Result<()> {
        if self.pending.is_some() {
            return Err("previous directory scan pending; retry Tab".into());
        }
        self.requests
            .try_send((id, text))
            .map_err(|error| error.to_string())?;
        self.pending = Some(id);
        Ok(())
    }

    pub(crate) fn poll(&mut self) -> Option<(u64, Result<Matches>)> {
        let id = self.pending?;
        let reply = match self.replies.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => (id, Err("directory worker disconnected".into())),
        };
        self.pending = None;
        Some(reply)
    }

    pub(crate) fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

pub(crate) fn scan(text: &str) -> Result<Matches> {
    if text.len() > PATH_BYTES || text.as_bytes().contains(&0) {
        return Err("invalid path length or NUL".into());
    }
    let end = text.rfind('/').map_or(0, |i| i + 1);
    let base = text.get(..end).ok_or("path boundary")?;
    let prefix = text.get(end..).ok_or("path boundary")?;
    let directory = if base.is_empty() { "." } else { base };
    let mut paths = Vec::new();
    let mut name_bytes = 0usize;
    let mut skipped = 0usize;
    for (index, entry) in std::fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .enumerate()
    {
        if index == ENTRIES {
            return Err("directory exceeds 4096 entries; use a smaller directory".into());
        }
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        name_bytes = name_bytes
            .checked_add(name.len())
            .ok_or("directory name budget")?;
        if name_bytes > NAME_BYTES {
            return Err("directory name budget exceeded".into());
        }
        let Some(name) = name.to_str() else {
            skipped += 1;
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        if paths.len() == MATCHES {
            return Err("more than 128 matches; type a longer prefix".into());
        }
        let is_dir = entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir();
        let path = format!("{base}{name}{}", if is_dir { "/" } else { "" });
        if path.len() > PATH_BYTES {
            return Err("completed path exceeds 4096 bytes".into());
        }
        paths.push(path);
    }
    paths.sort_unstable();
    if paths.is_empty() {
        return Err(format!("no matches ({skipped} non-UTF-8 names omitted)"));
    }
    Ok(Matches {
        paths,
        selected: None,
        skipped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Directory(std::path::PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-editor-completion-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn prefix(&self, leaf: &str) -> String {
            self.0.join(leaf).to_str().unwrap().into()
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn literal_sorted_matches_common_prefix_cycles_and_three_row_pages() {
        let directory = Directory::new();
        for name in ["alpé", "alpê", "alpha", "alps", "other", ".hidden"] {
            std::fs::write(directory.0.join(name), b"keep").unwrap();
        }
        let mut matches = scan(&directory.prefix("al")).unwrap();
        assert_eq!(matches.prefix(), directory.prefix("alp"));
        assert_eq!(matches.cycle(true), Some(directory.prefix("alpê").as_str()));
        assert_eq!(matches.page().count(), 1);
        assert_eq!(
            matches.cycle(false),
            Some(directory.prefix("alpha").as_str())
        );
        assert_eq!(matches.page().count(), 3);
        assert_eq!(
            scan(&directory.prefix("alpé")).unwrap().prefix(),
            directory.prefix("alpé")
        );
        assert_eq!(scan(&directory.prefix("alp")).unwrap().paths.len(), 4);
        assert_eq!(scan(&directory.prefix(".")).unwrap().paths.len(), 1);
        let state = State::Ready(matches);
        assert!(state.fields().contains("completion-count=4"));
        assert_eq!(state.fields().matches("completion-item=").count(), 3);
        assert!(state.notice().contains("alpé"));
        let unicode = Matches {
            paths: vec!["é".into(), "ê".into()],
            selected: None,
            skipped: 0,
        };
        assert_eq!(unicode.prefix(), "");
    }

    #[test]
    fn directories_have_slashes_symlinks_do_not_and_non_utf8_is_explicitly_omitted() {
        use std::os::unix::ffi::OsStringExt;
        let directory = Directory::new();
        std::fs::create_dir(directory.0.join("dir")).unwrap();
        std::os::unix::fs::symlink("dir", directory.0.join("link")).unwrap();
        std::fs::write(
            directory
                .0
                .join(std::ffi::OsString::from_vec(b"bad-\xff".to_vec())),
            b"raw",
        )
        .unwrap();
        let matches = scan(&directory.prefix("")).unwrap();
        assert_eq!(matches.skipped, 1);
        assert_eq!(
            matches.paths,
            vec![directory.prefix("dir/"), directory.prefix("link")]
        );
        let one = scan(&directory.prefix("di")).unwrap();
        assert!(one.descend(one.prefix()));
        assert!(!scan(&directory.prefix("li"))
            .unwrap()
            .descend(&directory.prefix("link")));
    }

    #[test]
    fn refuses_limits_errors_and_never_returns_a_partial_unique_match() {
        let directory = Directory::new();
        assert!(scan(&directory.prefix("missing/")).is_err());
        assert!(scan(&directory.prefix("nope")).is_err());
        assert!(scan("\0").is_err());
        assert!(scan(&"x".repeat(4097)).is_err());
        for index in 0..129 {
            std::fs::write(directory.0.join(format!("item{index:04}")), b"").unwrap();
        }
        assert!(scan(&directory.prefix("item"))
            .err()
            .unwrap()
            .contains("128"));
        assert_eq!(scan(&directory.prefix("item0000")).unwrap().paths.len(), 1);
        for index in 129..4097 {
            std::fs::write(directory.0.join(format!("item{index:04}")), b"").unwrap();
        }
        assert!(scan(&directory.prefix("item0000"))
            .err()
            .unwrap()
            .contains("4096"));
    }

    #[test]
    fn worker_has_one_slot_and_preserves_request_identity() {
        let directory = Directory::new();
        std::fs::write(directory.0.join("file"), b"").unwrap();
        let mut worker = Worker::start().unwrap();
        worker.request(7, directory.prefix("fi")).unwrap();
        assert!(worker.request(8, directory.prefix("fi")).is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some((id, result)) = worker.poll() {
                assert_eq!(id, 7);
                assert_eq!(result.unwrap().prefix(), directory.prefix("file"));
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(!worker.pending());
        assert!(worker.poll().is_none());
    }
}
