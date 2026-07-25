//! Shared `/proc` access. Centralising the read keeps every applet's error
//! string in one shape (`<path>: <errno text>`).

pub fn read(path: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

/// Value of a `Key:  <n> kB` line from /proc/meminfo, in the unit procfs reports
/// (KiB for every field td reads). The prefix match is anchored, so `Cached`
/// does not match `SwapCached`, and the `:` check rejects a longer key that
/// merely starts with `key`.
pub fn meminfo_field(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        return rest.split_whitespace().next().and_then(|t| t.parse().ok());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "MemTotal:       16316816 kB\n\
                          MemFree:          204552 kB\n\
                          MemAvailable:   14003204 kB\n\
                          Buffers:          573452 kB\n\
                          Cached:         12664636 kB\n\
                          SwapCached:            0 kB\n\
                          SReclaimable:     640380 kB\n\
                          Shmem:             81920 kB\n\
                          SwapTotal:             0 kB\n\
                          SwapFree:              0 kB\n";

    #[test]
    fn reads_each_declared_field() {
        assert_eq!(meminfo_field(SAMPLE, "MemTotal"), Some(16316816));
        assert_eq!(meminfo_field(SAMPLE, "MemAvailable"), Some(14003204));
        assert_eq!(meminfo_field(SAMPLE, "SReclaimable"), Some(640380));
        assert_eq!(meminfo_field(SAMPLE, "SwapFree"), Some(0));
    }

    /// The bug this guards: an unanchored or colon-less match would read
    /// `SwapCached`'s value for `Cached`, silently understating buff/cache.
    #[test]
    fn cached_does_not_match_swapcached() {
        assert_eq!(meminfo_field(SAMPLE, "Cached"), Some(12664636));
    }

    #[test]
    fn absent_or_malformed_keys_are_none() {
        assert_eq!(meminfo_field(SAMPLE, "Nonexistent"), None);
        assert_eq!(meminfo_field("MemTotal: notanumber kB\n", "MemTotal"), None);
        assert_eq!(meminfo_field("MemTotal\n", "MemTotal"), None);
    }
}
