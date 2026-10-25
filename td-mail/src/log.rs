use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 16 * 1024;
const ROLLOVER: &[u8] = b"[LOG] Size limit reached; older log entries discarded.\n";

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

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
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        if let Ok(mut guard) = LOG_FILE.lock() {
            *guard = Some(file);
        }
    }
}

/// Truncate the log file to empty. Safe to call before startup.
pub fn clear() -> Result<(), String> {
    let path = log_path().ok_or_else(|| td_ui::xdg::Base::State.missing())?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create log dir: {}", e))?;
    }

    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| format!("Failed to clear log file {}: {}", path.display(), e))?;

    if let Ok(mut guard) = LOG_FILE.lock() {
        *guard = None;
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
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(ref mut f) = *guard {
            let _ = append_bounded(f, line.as_bytes(), MAX_LOG_BYTES);
        }
    }
}

// A nonblocking OS lock also serializes independent td-mail processes. Keep
// the same inode on rollover so existing writers cannot retain an old log.
fn append_bounded(file: &mut File, line: &[u8], limit: u64) -> std::io::Result<()> {
    if file.try_lock().is_err() {
        return Ok(());
    }
    let result = (|| {
        let length = u64::try_from(line.len()).unwrap_or(u64::MAX);
        if length.saturating_add(ROLLOVER.len() as u64) > limit {
            return Ok(());
        }
        if file.metadata()?.len().saturating_add(length) > limit {
            file.set_len(0)?;
            file.write_all(ROLLOVER)?;
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
    fn existing_oversized_log_and_two_writers_remain_bounded() -> std::io::Result<()> {
        let directory = td_fs::TempDir::new("mail-bounded-log")?;
        let path = directory.path().join("log");
        let mut first = OpenOptions::new().create(true).append(true).open(&path)?;
        let mut second = OpenOptions::new().append(true).open(&path)?;
        first.set_len(4096)?;
        append_bounded(&mut first, b"first\n", 128)?;
        assert_eq!(fs::read(&path)?, [ROLLOVER, b"first\n"].concat());
        for _ in 0..20 {
            append_bounded(&mut second, b"second writer\n", 128)?;
            assert!(first.metadata()?.len() <= 128);
        }
        first.lock()?;
        let before = fs::read(&path)?;
        append_bounded(&mut second, b"must skip\n", 128)?;
        assert_eq!(fs::read(&path)?, before);
        first.unlock()?;
        append_bounded(&mut second, b"after lock\n", 128)?;
        assert!(fs::read(&path)?.ends_with(b"after lock\n"));
        Ok(())
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
