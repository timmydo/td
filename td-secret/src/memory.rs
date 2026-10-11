//! The memory a process holding a key, a PIN or a passphrase requires: no
//! active swap where the kernel has swap at all, and a zero core-dump soft
//! limit. Pure over `/proc/swaps` and `/proc/self/limits` as the caller
//! read them, so each consumer reads them under its own file rules:
//! td-secret's store, with every crate compiling it, and td-install's
//! storage worker (td-install/DESIGN.md, "Storage operation worker").

use std::io;

pub(crate) fn check_swap_state(swaps: io::Result<String>) -> Result<(), String> {
    let swaps = match swaps {
        Ok(swaps) => swaps,
        // Linux registers /proc/swaps only when CONFIG_SWAP is enabled.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
    };
    if swaps.lines().next().is_none() || swaps.lines().skip(1).any(|line| !line.trim().is_empty()) {
        return Err("TPM credential release requires swap to be disabled".into());
    }
    Ok(())
}

/// `operation`, once the swap and core-dump state allow it.
pub(crate) fn with_memory_state<T>(
    swaps: io::Result<String>,
    limits: io::Result<String>,
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    check_swap_state(swaps)?;
    check_core_limit(&limits.map_err(|e| e.to_string())?)?;
    operation()
}

pub(crate) fn check_core_limit(limits: &str) -> Result<(), String> {
    let core = limits
        .lines()
        .find_map(|line| line.strip_prefix("Max core file size"));
    if core.and_then(|line| line.split_whitespace().next()) != Some("0") {
        return Err("TPM credential access requires a zero core-dump soft limit".into());
    }
    Ok(())
}
