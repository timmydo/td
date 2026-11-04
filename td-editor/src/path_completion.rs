//! Bounded literal-path completion. Filesystem work never runs on the UI thread.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};

type Result<T> = std::result::Result<T, String>;
const PATH_BYTES: usize = 4096;
const ENTRIES: usize = 4096;
const NAME_BYTES: usize = 1024 * 1024;
pub(crate) const PAGE_ROWS: usize = 12;

pub(crate) struct Matches {
    paths: Vec<String>,
    selected: Option<usize>,
    skipped: usize,
}

impl Matches {
    pub(crate) fn encode(&self, w: &mut td_remote::codec::Writer) {
        w.count(self.paths.len());
        for path in &self.paths {
            w.bytes(path.as_bytes());
        }
        w.u64(u64::try_from(self.skipped).unwrap_or(u64::MAX));
    }

    pub(crate) fn decode(
        r: &mut td_remote::codec::Reader<'_>,
    ) -> std::result::Result<Self, td_remote::Error> {
        let count = r.count(ENTRIES)?;
        let mut paths = Vec::with_capacity(count);
        let mut names = 0usize;
        for _ in 0..count {
            let path = r.text(PATH_BYTES)?;
            names = names.saturating_add(path.len());
            if names > ENTRIES.saturating_mul(PATH_BYTES) || path.contains('\0') {
                return Err(td_remote::Error::Protocol("a completion's path".into()));
            }
            paths.push(path.to_string());
        }
        let skipped = usize::try_from(r.u64()?).unwrap_or(usize::MAX);
        Ok(Self {
            paths,
            selected: None,
            skipped,
        })
    }

    /// Whether these could be completions of `text`, as a local scan's always
    /// are: a far side's replace what the person typed.
    pub(crate) fn complete(&self, text: &str) -> bool {
        !self.paths.is_empty() && self.paths.iter().all(|path| path.starts_with(text))
    }

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

    pub(crate) fn page_by(&mut self, backward: bool, rows: usize) -> Option<&str> {
        let rows = rows.clamp(1, PAGE_ROWS);
        let last = self.paths.len().checked_sub(1)?;
        let current = self.selected.unwrap_or(0);
        let next = if backward {
            current.saturating_sub(rows)
        } else {
            current.saturating_add(rows).min(last)
        };
        self.selected = Some(next);
        self.paths.get(next).map(String::as_str)
    }

    pub(crate) fn count(&self) -> usize {
        self.paths.len()
    }

    fn page(&self, rows: usize) -> impl Iterator<Item = (usize, &str)> {
        let rows = rows.clamp(1, PAGE_ROWS);
        let start = self.selected.unwrap_or(0) / rows * rows;
        self.paths
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
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
    pub(crate) fn notice(&self, rows: usize) -> String {
        match self {
            Self::Idle => "Tab: complete; Up/Down: choose matches".into(),
            Self::Pending(_) => "Reading directory for completion...".into(),
            Self::Failed(detail) => format!(
                "Completion: {}",
                detail.chars().take(100).collect::<String>()
            ),
            Self::Ready(matches) => {
                let mut text = format!(
                    "{}-{} / {} matches; PgUp/PgDn; {} non-UTF-8 omitted",
                    matches.page(rows).next().map_or(0, |(i, _)| i + 1),
                    matches.page(rows).last().map_or(0, |(i, _)| i + 1),
                    matches.paths.len(),
                    matches.skipped
                );
                for (index, path) in matches.page(rows) {
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

    pub(crate) fn fields(&self, rows: usize) -> String {
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
                fields.push_str(&format!(
                    "\tcompletion-page-size={}",
                    rows.clamp(1, PAGE_ROWS)
                ));
                for (index, path) in matches.page(rows) {
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
    requests: SyncSender<(u64, String, bool)>,
    replies: Receiver<(u64, Result<Matches>)>,
    pending: Option<u64>,
}

impl Worker {
    pub(crate) fn start() -> Result<Self> {
        Self::start_with(|text, _| scan(text))
    }

    /// Whichever machine the window's files are on: a worker's wait for a far
    /// side is the same to the window as its wait for a slow disk.
    pub(crate) fn start_for(
        remote: Option<&std::sync::Arc<crate::remote::Remote>>,
    ) -> Result<Self> {
        match remote {
            None => Self::start(),
            Some(remote) => {
                let remote = std::sync::Arc::clone(remote);
                Self::start_with(move |text, here| {
                    if here {
                        scan(text)
                    } else {
                        remote.complete(text)
                    }
                })
            }
        }
    }

    fn start_with(scan: impl Fn(&str, bool) -> Result<Matches> + Send + 'static) -> Result<Self> {
        let (requests, input) = mpsc::sync_channel::<(u64, String, bool)>(1);
        let (output, replies) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("editor-path-completion".into())
            .spawn(move || {
                while let Ok((id, text, here)) = input.recv() {
                    if output.send((id, scan(&text, here))).is_err() {
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

    /// `here` asks of this machine even in a window of another's files: a
    /// dictionary is the window's own.
    pub(crate) fn request(&mut self, id: u64, text: String, here: bool) -> Result<()> {
        if self.pending.is_some() {
            return Err("previous directory scan pending; retry Tab".into());
        }
        self.requests
            .try_send((id, text, here))
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
        assert_eq!(matches.page(3).count(), 1);
        assert_eq!(
            matches.cycle(false),
            Some(directory.prefix("alpha").as_str())
        );
        assert_eq!(matches.page(3).count(), 3);
        assert_eq!(
            scan(&directory.prefix("alpé")).unwrap().prefix(),
            directory.prefix("alpé")
        );
        assert_eq!(scan(&directory.prefix("alp")).unwrap().paths.len(), 4);
        assert_eq!(scan(&directory.prefix(".")).unwrap().paths.len(), 1);
        let state = State::Ready(matches);
        assert!(state.fields(3).contains("completion-count=4"));
        assert_eq!(state.fields(3).matches("completion-item=").count(), 3);
        assert!(state.notice(3).contains("alpé"));
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
        let mut matches = scan(&directory.prefix("item")).unwrap();
        assert_eq!(matches.count(), 129);
        assert_eq!(matches.page(12).count(), 12);
        assert_eq!(
            matches.page_by(false, 12),
            Some(directory.prefix("item0012").as_str())
        );
        assert_eq!(matches.page(12).next().unwrap().0, 12);
        assert_eq!(
            matches.page_by(true, 12),
            Some(directory.prefix("item0000").as_str())
        );
        assert_eq!(
            matches.page_by(false, usize::MAX),
            Some(directory.prefix("item0012").as_str())
        );
        assert_eq!(
            matches.page_by(true, 0),
            Some(directory.prefix("item0011").as_str())
        );
        for _ in 0..12 {
            matches.page_by(false, 12);
        }
        assert_eq!(matches.selected, Some(128));
        assert_eq!(matches.page(12).last().unwrap().0, 128);
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
        worker.request(7, directory.prefix("fi"), false).unwrap();
        assert!(worker.request(8, directory.prefix("fi"), false).is_err());
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
