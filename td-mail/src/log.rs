use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const ROLLOVER: &[u8] = b"[LOG] Size limit reached; retaining recent log entries.\n";

enum Sink {
    Disabled,
    Failed,
    File(File),
}
static LOG_FILE: Mutex<Sink> = Mutex::new(Sink::Disabled);

/// `td-mail/td-mail.log` under the state home; none without one, and
/// then nothing is logged.
pub fn log_path() -> Option<PathBuf> {
    td_ui::xdg::from_env(td_ui::xdg::Base::State).map(|dir| dir.join("td-mail").join("td-mail.log"))
}

/// Initialize the log file. Call once at startup.
pub fn init() {
    let Some(path) = log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let sink = match open_log(&path) {
        Ok(file) => Sink::File(file),
        Err(error) => {
            eprintln!("td-mail log open failed for {}: {error}", path.display());
            Sink::Failed
        }
    };
    match LOG_FILE.lock() {
        Ok(mut guard) => *guard = sink,
        Err(error) => eprintln!("td-mail log initialization failed: {error}"),
    }
}

/// Truncate the log file to empty. Safe to call before startup.
pub fn clear() -> Result<(), String> {
    let path = log_path().ok_or_else(|| td_ui::xdg::Base::State.missing())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create log dir: {}", e))?;
    }

    let file = open_log(&path)
        .map_err(|e| format!("Failed to open log file {}: {}", path.display(), e))?;
    file.lock()
        .map_err(|e| format!("Failed to lock log: {e}"))?;
    let result = file.set_len(0);
    let unlock = file.unlock();
    result
        .and(unlock)
        .map_err(|e| format!("Failed to clear log: {e}"))?;

    if let Ok(mut guard) = LOG_FILE.lock() {
        *guard = Sink::Disabled;
    }

    Ok(())
}

/// Write a log line to the file.
pub fn write_log(level: &str, msg: &str) {
    let ts = timestamp();
    let mut end = msg.len().min(MAX_LINE_BYTES);
    while !msg.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let text = msg.get(..end).unwrap_or_default();
    let suffix = if end < msg.len() { " [truncated]" } else { "" };
    let line = format!("[{}] [{}] {}{}\n", ts, level, text, suffix);
    match LOG_FILE.lock() {
        Ok(mut guard) => match &mut *guard {
            Sink::File(file) => {
                if let Err(error) = append_bounded(file, line.as_bytes(), MAX_LOG_BYTES) {
                    eprintln!("td-mail log write failed: {error}; {line}");
                }
            }
            Sink::Failed => eprintln!("td-mail log unavailable; {line}"),
            Sink::Disabled => {}
        },
        Err(error) => eprintln!("td-mail log lock failed: {error}; {line}"),
    }
}

fn open_log(path: &Path) -> std::io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

// Serialize independent processes without dropping contending warnings/errors.
// Keep the inode so existing writers observe the same compacted history.
fn append_bounded(file: &mut File, line: &[u8], limit: u64) -> std::io::Result<()> {
    file.lock()?;
    let result = (|| {
        let length = u64::try_from(line.len()).unwrap_or(u64::MAX);
        let overhead = length.saturating_add(ROLLOVER.len() as u64);
        if overhead > limit {
            return Err(std::io::Error::other("log entry exceeds file limit"));
        }
        let old_length = file.metadata()?.len();
        if old_length.saturating_add(length) > limit {
            let keep = (limit / 2).min(limit - overhead).min(old_length);
            let count = usize::try_from(keep).map_err(std::io::Error::other)?;
            let mut recent = Vec::new();
            recent
                .try_reserve_exact(count)
                .map_err(std::io::Error::other)?;
            recent.resize(count, 0);
            let offset = old_length - keep;
            file.read_exact_at(&mut recent, offset)?;
            let start = if offset == 0 {
                0
            } else {
                recent
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(recent.len(), |index| index + 1)
            };
            let retained = recent
                .get(start..)
                .ok_or_else(|| std::io::Error::other("invalid log boundary"))?;
            file.set_len(0)?;
            file.write_all(ROLLOVER)?;
            file.write_all(retained)?;
        }
        file.write_all(line)
    })();
    let unlock = file.unlock();
    result.and(unlock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_log_keeps_recent_complete_lines_and_private_permissions() -> std::io::Result<()> {
        let directory = td_fs::TempDir::new("mail-bounded-log")?;
        let path = directory.path().join("log");
        fs::write(&path, b"old data\n".repeat(100))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
        let mut file = open_log(&path)?;
        file.write_all(b"[WARN] recent warning\n")?;
        append_bounded(&mut file, b"new entry\n", 256)?;
        let contents = fs::read_to_string(&path)?;
        assert!(contents.contains("[WARN] recent warning\n"));
        assert!(contents.ends_with("new entry\n"));
        assert!(contents.starts_with(std::str::from_utf8(ROLLOVER).map_err(std::io::Error::other)?));
        assert!(file.metadata()?.len() <= 256);
        assert_eq!(file.metadata()?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn contending_writer_waits_then_preserves_its_warning() -> std::io::Result<()> {
        let directory = td_fs::TempDir::new("mail-log-contention")?;
        let path = directory.path().join("log");
        let first = open_log(&path)?;
        let mut second = open_log(&path)?;
        first.lock()?;
        let (started, starting) = std::sync::mpsc::channel();
        let (completed, completion) = std::sync::mpsc::channel();
        std::thread::scope(|scope| -> std::io::Result<()> {
            let worker = scope.spawn(move || {
                let _ = started.send(());
                let result = append_bounded(&mut second, b"[WARN] contention\n", 256);
                let _ = completed.send(());
                result
            });
            starting.recv().map_err(std::io::Error::other)?;
            let blocked = completion
                .recv_timeout(std::time::Duration::from_millis(25))
                .is_err();
            first.unlock()?;
            worker
                .join()
                .map_err(|_| std::io::Error::other("log writer failed"))??;
            assert!(blocked);
            assert!(fs::read(&path)?.ends_with(b"[WARN] contention\n"));
            Ok(())
        })
    }
}

/// The current instant as ISO 8601 in UTC, to the millisecond.
fn timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let c = td_civil::unix_to_civil_utc(i64::try_from(now.as_secs()).unwrap_or(i64::MAX));
    format!(
        "{}T{}.{:03}Z",
        td_civil::format_ymd(&c),
        td_civil::format_hms(&c),
        now.subsec_millis()
    )
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::write_log("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::write_log("DEBUG", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::write_log("ERROR", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::write_log("WARN", &format!($($arg)*))
    };
}
