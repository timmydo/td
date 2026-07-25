//! `free` — memory summary from /proc/meminfo, in procps' column set.

pub enum Unit {
    Bytes,
    Kib,
    Mib,
    Gib,
    Human,
}

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut unit = Unit::Kib;
    for a in args {
        match a.as_str() {
            "-b" | "--bytes" => unit = Unit::Bytes,
            "-k" | "--kibi" => unit = Unit::Kib,
            "-m" | "--mebi" => unit = Unit::Mib,
            "-g" | "--gibi" => unit = Unit::Gib,
            "-h" | "--human" => unit = Unit::Human,
            other => {
                return Err(format!(
                    "unrecognised option '{other}'\nusage: free [-b|-k|-m|-g|-h]"
                ))
            }
        }
    }
    let text = crate::procfs::read("/proc/meminfo")?;
    let report = Report::from_meminfo(&text)?;
    crate::emit(&report.render(&unit))?;
    Ok(0)
}

/// Every field is KiB, as procfs reports it.
pub struct Report {
    pub mem_total: u64,
    pub mem_free: u64,
    pub mem_available: u64,
    pub buff_cache: u64,
    pub shared: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

impl Report {
    pub fn from_meminfo(text: &str) -> Result<Report, String> {
        let get = |k: &str| crate::procfs::meminfo_field(text, k);
        let need =
            |k: &str| get(k).ok_or_else(|| format!("/proc/meminfo: missing or malformed '{k}'"));
        let mem_total = need("MemTotal")?;
        let mem_free = need("MemFree")?;
        Ok(Report {
            mem_total,
            mem_free,
            // MemAvailable predates every kernel td ships, but degrade rather
            // than fail: an older kernel should still get a usable `free`.
            mem_available: get("MemAvailable").unwrap_or(mem_free),
            buff_cache: get("Buffers")
                .unwrap_or(0)
                .saturating_add(get("Cached").unwrap_or(0))
                .saturating_add(get("SReclaimable").unwrap_or(0)),
            shared: get("Shmem").unwrap_or(0),
            swap_total: get("SwapTotal").unwrap_or(0),
            swap_free: get("SwapFree").unwrap_or(0),
        })
    }

    /// procps' definition since 3.3.10: used is what is NOT available, which is
    /// not the same as `total - free - buff/cache` — MemAvailable also discounts
    /// the watermark reserve and the part of the cache the kernel cannot actually
    /// reclaim. procps falls back to `total - free` when available exceeds total.
    pub fn used(&self) -> u64 {
        if self.mem_available > self.mem_total {
            return self.mem_total.saturating_sub(self.mem_free);
        }
        self.mem_total.saturating_sub(self.mem_available)
    }

    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }

    pub fn render(&self, unit: &Unit) -> String {
        let s = |kib: u64| scale(kib, unit);
        let mut out = format!(
            "{:7}{:>12}{:>12}{:>12}{:>12}{:>12}{:>12}\n",
            "", "total", "used", "free", "shared", "buff/cache", "available"
        );
        out.push_str(&format!(
            "{:7}{:>12}{:>12}{:>12}{:>12}{:>12}{:>12}\n",
            "Mem:",
            s(self.mem_total),
            s(self.used()),
            s(self.mem_free),
            s(self.shared),
            s(self.buff_cache),
            s(self.mem_available),
        ));
        out.push_str(&format!(
            "{:7}{:>12}{:>12}{:>12}\n",
            "Swap:",
            s(self.swap_total),
            s(self.swap_used()),
            s(self.swap_free),
        ));
        out
    }
}

fn scale(kib: u64, unit: &Unit) -> String {
    match unit {
        Unit::Bytes => format!("{}", kib.saturating_mul(1024)),
        Unit::Kib => format!("{kib}"),
        Unit::Mib => format!("{}", kib / 1024),
        Unit::Gib => format!("{}", kib / (1024 * 1024)),
        Unit::Human => human(kib),
    }
}

/// One decimal place against binary steps, the shape `free -h` prints. procps
/// formats with `%.1f`, which ROUNDS -- truncating shows 1.9Gi where free shows
/// 2.0Gi. u128 because `bytes * 10` overflows u64 near the top of the range.
fn human(kib: u64) -> String {
    let bytes = kib.saturating_mul(1024);
    for (div, suffix) in [(1u64 << 30, "Gi"), (1 << 20, "Mi"), (1 << 10, "Ki")] {
        if bytes >= div {
            let (b, d) = (u128::from(bytes), u128::from(div));
            let tenths = (b * 10 + d / 2) / d;
            return format!("{}.{}{}", tenths / 10, tenths % 10, suffix);
        }
    }
    format!("{bytes}B")
}

#[cfg(test)]
mod tests {
    // The gate lints only non-test targets, but keep `cargo clippy --tests`
    // clean for local runs too.
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    const SAMPLE: &str = "MemTotal:       16316816 kB\n\
                          MemFree:          204552 kB\n\
                          MemAvailable:   14003204 kB\n\
                          Buffers:          573452 kB\n\
                          Cached:         12664636 kB\n\
                          SReclaimable:     640380 kB\n\
                          Shmem:             81920 kB\n\
                          SwapTotal:        1048572 kB\n\
                          SwapFree:         1048000 kB\n";

    fn sample() -> Report {
        Report::from_meminfo(SAMPLE).unwrap()
    }

    #[test]
    fn buff_cache_sums_buffers_cached_and_reclaimable() {
        assert_eq!(sample().buff_cache, 573452 + 12664636 + 640380);
    }

    /// procps computes used as `total - available`, NOT `total - free - cache`.
    /// The two differ by the unreclaimable part of the cache plus the watermark
    /// reserve -- here by more than 1.8 GiB, so getting it wrong is visible.
    #[test]
    fn used_is_total_minus_available() {
        let r = sample();
        assert_eq!(r.used(), 16316816 - 14003204);
        assert_ne!(r.used(), 16316816 - 204552 - (573452 + 12664636 + 640380));
        assert_eq!(r.swap_used(), 1048572 - 1048000);
    }

    /// procps' own fallback: when MemAvailable exceeds MemTotal (a kernel bug or
    /// a synthetic /proc), report `total - free` rather than saturating to 0.
    #[test]
    fn used_falls_back_when_available_exceeds_total() {
        let r = Report {
            mem_total: 100,
            mem_free: 90,
            mem_available: 5_000,
            buff_cache: 0,
            shared: 0,
            swap_total: 0,
            swap_free: 9_999,
        };
        assert_eq!(r.used(), 10);
        // Swap still saturates rather than underflow-panicking.
        assert_eq!(r.swap_used(), 0);
    }

    #[test]
    fn missing_memtotal_is_an_error_not_a_zero_row() {
        assert!(Report::from_meminfo("MemFree: 10 kB\n").is_err());
    }

    #[test]
    fn memavailable_falls_back_to_memfree() {
        let r = Report::from_meminfo("MemTotal: 100 kB\nMemFree: 40 kB\n").unwrap();
        assert_eq!(r.mem_available, 40);
    }

    #[test]
    fn scales_convert_from_kib() {
        assert_eq!(scale(2048, &Unit::Kib), "2048");
        assert_eq!(scale(2048, &Unit::Bytes), "2097152");
        assert_eq!(scale(2048, &Unit::Mib), "2");
        assert_eq!(scale(4 * 1024 * 1024, &Unit::Gib), "4");
        assert_eq!(human(1536), "1.5Mi");
        assert_eq!(human(512), "512.0Ki");
        assert_eq!(human(0), "0B");
    }

    /// `%.1f` rounds; truncating would print 1.9Gi where procps prints 2.0Gi.
    #[test]
    fn human_rounds_like_procps_rather_than_truncating() {
        assert_eq!(human(2 * 1024 * 1024 - 1024), "2.0Gi");
        assert_eq!(human(1024 + 512), "1.5Mi");
        // ...and does not round a value that is genuinely below the step.
        assert_eq!(human(1024 + 100), "1.1Mi");
        // The top of the u64 range must not overflow the *10 in the rounding.
        assert!(human(u64::MAX).ends_with("Gi"));
    }

    #[test]
    fn render_emits_a_header_and_both_rows() {
        let text = sample().render(&Unit::Kib);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("total") && lines[0].contains("available"));
        assert!(lines[1].starts_with("Mem:"));
        assert!(lines[2].starts_with("Swap:"));
    }
}
