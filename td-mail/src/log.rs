use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

/// Return the log file path: $XDG_STATE_HOME/td-mail/td-mail.log
pub fn log_path() -> PathBuf {
    let state_dir = if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        PathBuf::from(xdg)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local").join("state")
    } else {
        PathBuf::from(".")
    };
    state_dir.join("td-mail").join("td-mail.log")
}

/// Initialize the log file. Call once at startup.
pub fn init() {
    let path = log_path();
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
    let path = log_path();
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
    let line = format!("[{}] [{}] {}\n", ts, level, msg);
    if let Ok(mut guard) = LOG_FILE.lock() {
        if let Some(ref mut f) = *guard {
            let _ = f.write_all(line.as_bytes());
        }
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
