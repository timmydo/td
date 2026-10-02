//! Test-only helpers the shared modules' own tests name (`td_fetch`'s
//! socket tests take `env_lock`), as td-news's `testing` gives them.

/// Held by every test that points an XDG variable at a directory of its
/// own. The environment is one per process and the tests run in
/// parallel: two setting it at once would open under each other's
/// directory, and one of those directories is about to be removed.
pub fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
