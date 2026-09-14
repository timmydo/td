//! Bounded byte parsers, with no paths, clocks or platform encoding.
pub const AGGREGATE_BYTES: usize = 1024 * 1024;
pub const PROCESS_BYTES: usize = 64 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Limit,
    Malformed,
    Overflow,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Limit => "source exceeds byte limit",
            Self::Malformed => "malformed source",
            Self::Overflow => "counter overflow",
        })
    }
}
impl std::error::Error for Error {}
pub fn unsigned(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    bytes.iter().try_fold(0u64, |n, b| {
        if !b.is_ascii_digit() {
            return None;
        }
        n.checked_mul(10)?.checked_add(u64::from(*b - b'0'))
    })
}
fn words(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes
        .split(u8::is_ascii_whitespace)
        .filter(|part| !part.is_empty())
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Process<'a> {
    pub pid: u32,
    pub name: &'a [u8],
    pub state: u8,
    pub parent: Option<u32>,
    pub start_ticks: u64,
    pub user_ticks: Option<u64>,
    pub system_ticks: Option<u64>,
    pub rss_pages: Option<u64>,
}
pub fn process(bytes: &[u8]) -> Result<Process<'_>, Error> {
    if bytes.len() > PROCESS_BYTES {
        return Err(Error::Limit);
    }
    let open = bytes
        .iter()
        .position(|b| *b == b'(')
        .ok_or(Error::Malformed)?;
    let close = bytes
        .iter()
        .rposition(|b| *b == b')')
        .filter(|close| *close > open)
        .ok_or(Error::Malformed)?;
    let prefix = bytes.get(..open).ok_or(Error::Malformed)?;
    let mut prefix = words(prefix);
    let pid = prefix
        .next()
        .and_then(unsigned)
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or(Error::Malformed)?;
    if prefix.next().is_some() {
        return Err(Error::Malformed);
    }
    let name = bytes.get(open + 1..close).ok_or(Error::Malformed)?;
    let mut fields = words(bytes.get(close + 1..).ok_or(Error::Malformed)?);
    let state = fields
        .next()
        .filter(|field| field.len() == 1)
        .and_then(|field| field.first())
        .copied()
        .filter(|state| state.is_ascii_alphabetic())
        .ok_or(Error::Malformed)?;
    let parent = fields
        .next()
        .and_then(unsigned)
        .and_then(|n| u32::try_from(n).ok());
    // The iterator now starts at field 5; utime is field 14.
    let user_ticks = fields.nth(9).and_then(unsigned);
    let system_ticks = fields.next().and_then(unsigned);
    let start_ticks = fields.nth(6).and_then(unsigned).ok_or(Error::Malformed)?;
    let rss_pages = fields.nth(1).and_then(unsigned);
    Ok(Process {
        pid,
        name,
        state,
        parent,
        start_ticks,
        user_ticks,
        system_ticks,
        rss_pages,
    })
}
pub fn real_uid(bytes: &[u8]) -> Result<Option<u32>, Error> {
    if bytes.len() > PROCESS_BYTES {
        return Err(Error::Limit);
    }
    let mut uid = None;
    let mut seen = false;
    for line in bytes.split(|b| *b == b'\n') {
        if let Some(tail) = line.strip_prefix(b"Uid:") {
            if seen {
                return Err(Error::Malformed);
            }
            seen = true;
            uid = words(tail)
                .next()
                .and_then(unsigned)
                .and_then(|n| u32::try_from(n).ok());
        }
    }
    Ok(uid)
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Cpu {
    pub counters: [u64; 8],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CpuUsage {
    pub busy: u64,
    pub iowait: u64,
    pub steal: u64,
}
impl Cpu {
    pub fn parse(line: &[u8]) -> Result<(Option<u32>, Self), Error> {
        if line.len() > AGGREGATE_BYTES {
            return Err(Error::Limit);
        }
        let mut fields = words(line);
        let label = fields.next().ok_or(Error::Malformed)?;
        let id = if label == b"cpu" {
            None
        } else {
            Some(
                label
                    .strip_prefix(b"cpu")
                    .and_then(unsigned)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(Error::Malformed)?,
            )
        };
        let mut counters = [0; 8];
        for value in &mut counters {
            *value = fields.next().and_then(unsigned).ok_or(Error::Malformed)?;
        }
        Ok((id, Self { counters }))
    }
    pub fn since(self, old: Self) -> Option<CpuUsage> {
        let mut delta = [0; 8];
        for ((out, next), old) in delta.iter_mut().zip(self.counters).zip(old.counters) {
            *out = next.checked_sub(old)?;
        }
        let total = delta.iter().try_fold(0u64, |sum, n| sum.checked_add(*n))?;
        if total == 0 {
            return None;
        }
        let idle = *delta.get(3)?;
        let wait = *delta.get(4)?;
        let steal = *delta.get(7)?;
        let busy = total
            .checked_sub(idle)?
            .checked_sub(wait)?
            .checked_sub(steal)?;
        let percent = |n| u64::try_from(u128::from(n) * 10000 / u128::from(total)).ok();
        Some(CpuUsage {
            busy: percent(busy)?,
            iowait: percent(wait)?,
            steal: percent(steal)?,
        })
    }
}
/// Basis points of one logical CPU; elapsed nanoseconds and runtime ticks/sec.
pub fn process_cpu(
    next: Process<'_>,
    old: Process<'_>,
    ticks_per_second: u64,
    elapsed_ns: u64,
) -> Option<u64> {
    if next.pid != old.pid
        || next.start_ticks != old.start_ticks
        || ticks_per_second == 0
        || elapsed_ns == 0
    {
        return None;
    }
    let delta = next
        .user_ticks?
        .checked_sub(old.user_ticks?)?
        .checked_add(next.system_ticks?.checked_sub(old.system_ticks?)?)?;
    let numerator = u128::from(delta) * 10000 * 1_000_000_000;
    let denominator = u128::from(ticks_per_second) * u128::from(elapsed_ns);
    u64::try_from(numerator / denominator).ok()
}
pub fn resident_bytes(pages: Option<u64>, page_size: u64) -> Option<u64> {
    if page_size == 0 {
        return None;
    }
    pages?.checked_mul(page_size)
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Memory {
    pub total: Option<u64>,
    pub available: Option<u64>,
    pub swap_total: Option<u64>,
    pub swap_free: Option<u64>,
}
impl Memory {
    pub fn used(self) -> Option<u64> {
        self.total?.checked_sub(self.available?)
    }
    pub fn swap_used(self) -> Option<u64> {
        self.swap_total?.checked_sub(self.swap_free?)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > AGGREGATE_BYTES {
            return Err(Error::Limit);
        }
        let mut result = Self::default();
        let mut seen = 0u8;
        for line in bytes.split(|b| *b == b'\n') {
            let mut fields = words(line);
            let (bit, slot) = match fields.next() {
                Some(b"MemTotal:") => (1, &mut result.total),
                Some(b"MemAvailable:") => (2, &mut result.available),
                Some(b"SwapTotal:") => (4, &mut result.swap_total),
                Some(b"SwapFree:") => (8, &mut result.swap_free),
                _ => continue,
            };
            if seen & bit != 0 {
                return Err(Error::Malformed);
            }
            seen |= bit;
            let value = fields
                .next()
                .and_then(unsigned)
                .and_then(|n| n.checked_mul(1024));
            *slot = if fields.next() == Some(&b"kB"[..]) && fields.next().is_none() {
                value
            } else {
                None
            };
        }
        Ok(result)
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Network<'a> {
    pub name: &'a [u8],
    pub received: u64,
    pub sent: u64,
}
pub fn network(line: &[u8]) -> Result<Network<'_>, Error> {
    if line.len() > AGGREGATE_BYTES {
        return Err(Error::Limit);
    }
    let colon = line
        .iter()
        .rposition(|b| *b == b':')
        .ok_or(Error::Malformed)?;
    let name = line.get(..colon).ok_or(Error::Malformed)?.trim_ascii();
    if name.is_empty() || name.len() > 128 {
        return Err(Error::Malformed);
    }
    let mut fields = words(line.get(colon + 1..).ok_or(Error::Malformed)?);
    let received = fields.next().and_then(unsigned).ok_or(Error::Malformed)?;
    let sent = fields.nth(7).and_then(unsigned).ok_or(Error::Malformed)?;
    Ok(Network {
        name,
        received,
        sent,
    })
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Disk<'a> {
    pub major: u32,
    pub minor: u32,
    pub name: &'a [u8],
    pub reads: u64,
    pub read_sectors: u64,
    pub writes: u64,
    pub written_sectors: u64,
    pub busy_ms: u64,
}
pub fn disk(line: &[u8]) -> Result<Disk<'_>, Error> {
    if line.len() > AGGREGATE_BYTES {
        return Err(Error::Limit);
    }
    let mut fields = words(line);
    let major = fields
        .next()
        .and_then(unsigned)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Error::Malformed)?;
    let minor = fields
        .next()
        .and_then(unsigned)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or(Error::Malformed)?;
    let name = fields
        .next()
        .filter(|name| !name.is_empty() && name.len() <= 128)
        .ok_or(Error::Malformed)?;
    let reads = fields.next().and_then(unsigned).ok_or(Error::Malformed)?;
    let read_sectors = fields.nth(1).and_then(unsigned).ok_or(Error::Malformed)?;
    let writes = fields.nth(1).and_then(unsigned).ok_or(Error::Malformed)?;
    let written_sectors = fields.nth(1).and_then(unsigned).ok_or(Error::Malformed)?;
    let busy_ms = fields.nth(2).and_then(unsigned).ok_or(Error::Malformed)?;
    Ok(Disk {
        major,
        minor,
        name,
        reads,
        read_sectors,
        writes,
        written_sectors,
        busy_ms,
    })
}
/// Checked units per second; disk callers pass 512 bytes per accounting sector.
pub fn rate(next: u64, previous: u64, units: u64, elapsed_ns: u64) -> Option<u64> {
    if elapsed_ns == 0 {
        return None;
    }
    let delta = next.checked_sub(previous)?;
    let numerator = u128::from(delta)
        .checked_mul(u128::from(units))?
        .checked_mul(1_000_000_000)?;
    u64::try_from(numerator / u128::from(elapsed_ns)).ok()
}
/// Escape process-controlled text into storage already charged by the caller.
/// Returns true when truncated. No allocation or partial scalar/escape occurs.
pub fn escaped_text(bytes: &[u8], output: &mut String, limit: usize) -> Result<bool, Error> {
    output.clear();
    if bytes.len() > PROCESS_BYTES
        || limit > 4096
        || output.capacity() < bytes.len().saturating_mul(4).min(limit)
    {
        return Err(Error::Limit);
    }
    fn append(
        chars: impl Iterator<Item = char> + Clone,
        output: &mut String,
        limit: usize,
    ) -> bool {
        let size = chars.clone().map(char::len_utf8).sum::<usize>();
        if output.len() + size > limit {
            return false;
        }
        output.extend(chars);
        true
    }
    let hex = |n: u8| char::from(b"0123456789abcdef".get(n as usize).copied().unwrap_or(b'?'));
    let mut rest = bytes;
    while !rest.is_empty() {
        let (text, invalid) = match std::str::from_utf8(rest) {
            Ok(text) => (text, 0),
            Err(error) => {
                let prefix = rest.get(..error.valid_up_to()).ok_or(Error::Malformed)?;
                (
                    std::str::from_utf8(prefix).map_err(|_| Error::Malformed)?,
                    error
                        .error_len()
                        .unwrap_or(rest.len() - error.valid_up_to()),
                )
            }
        };
        for ch in text.chars() {
            let format_control = matches!(ch,'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2060}'..='\u{206f}'|'\u{feff}');
            let fit = if ch.is_ascii_control() && !matches!(ch, '\n' | '\r' | '\t') {
                append(
                    ['\\', 'x', hex(ch as u8 >> 4), hex(ch as u8 & 15)].into_iter(),
                    output,
                    limit,
                )
            } else if ch.is_control() || format_control || ch == '\\' {
                append(ch.escape_default(), output, limit)
            } else {
                append(std::iter::once(ch), output, limit)
            };
            if !fit {
                return Ok(true);
            }
        }
        rest = rest.get(text.len()..).ok_or(Error::Malformed)?;
        for byte in rest.get(..invalid).ok_or(Error::Malformed)? {
            if !append(
                ['\\', 'x', hex(*byte >> 4), hex(*byte & 15)].into_iter(),
                output,
                limit,
            ) {
                return Ok(true);
            }
        }
        rest = rest.get(invalid..).ok_or(Error::Malformed)?;
    }
    Ok(false)
}
