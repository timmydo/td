use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static NEWS_LOGGER: OnceLock<Result<Mutex<File>, String>> = OnceLock::new();
static DEBUG_LOGGER: OnceLock<Result<Mutex<File>, String>> = OnceLock::new();

/// Where the logs live: a directory a test names, so a test that logs
/// writes nowhere near a person's own logs; otherwise the cache home.
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

fn log_dir() -> PathBuf {
    if let Some(dir) = LOG_DIR.get() {
        return dir.clone();
    }
    let xdg = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.cache", home)
    });
    PathBuf::from(xdg).join("tn")
}

/// Sends every log of this process to `dir`, once; a later call keeps the
/// first directory, so tests that share a process share it.
#[cfg(test)]
pub fn use_log_dir(dir: PathBuf) -> PathBuf {
    LOG_DIR.get_or_init(|| dir).clone()
}

pub fn news_log_path() -> PathBuf {
    log_dir().join("news.log")
}

pub fn debug_log_path() -> PathBuf {
    log_dir().join("debug.log")
}

pub fn init() -> Result<(), String> {
    let news_ok = match NEWS_LOGGER.get_or_init(|| open_logger(news_log_path())) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.clone()),
    };
    let debug_ok = match DEBUG_LOGGER.get_or_init(|| open_logger(debug_log_path())) {
        Ok(_) => Ok(()),
        Err(e) => Err(e.clone()),
    };

    match (news_ok, debug_ok) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(e),
    }
}

pub fn info(msg: impl AsRef<str>) {
    write_line(&DEBUG_LOGGER, debug_log_path, "INFO", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    write_line(&DEBUG_LOGGER, debug_log_path, "WARN", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    write_line(&DEBUG_LOGGER, debug_log_path, "ERROR", msg.as_ref());
}

pub fn news(msg: impl AsRef<str>) {
    write_line(&NEWS_LOGGER, news_log_path, "INFO", msg.as_ref());
}

fn open_logger(path: PathBuf) -> Result<Mutex<File>, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create log dir: {}", e))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open log file {}: {}", path.display(), e))?;
    Ok(Mutex::new(file))
}

fn write_line(
    logger_ref: &OnceLock<Result<Mutex<File>, String>>,
    path_fn: fn() -> PathBuf,
    level: &str,
    msg: &str,
) {
    let logger = match logger_ref.get_or_init(|| open_logger(path_fn())) {
        Ok(logger) => logger,
        Err(_) => return,
    };

    if let Ok(mut file) = logger.lock() {
        let timestamp = crate::feed::now_local_datetime_string();
        let message = msg.replace('\n', "\\n");
        let _ = writeln!(file, "{} [{}] {}", timestamp, level, message);
    }
}
