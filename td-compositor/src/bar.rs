use crate::ui;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tall enough for one line of 2x glyphs with a little air. The tiling area
/// is the output minus this, so the number is layout, not decoration.
pub const BAR_HEIGHT: usize = 24;
const TEXT_TOP: usize = 5;
const TEXT_LEFT: usize = 8;
const SCALE: usize = 2;
const SEPARATOR: &str = "  ";
pub(crate) const BACKGROUND: [u8; 4] = [0x18, 0x14, 0x20, 0];
pub(crate) const INK: [u8; 4] = [0xd0, 0xc8, 0xe0, 0];
/// Air either side of a workspace number inside its own cell. The cell is the
/// number and this twice, so a workspace costs the status line twenty pixels
/// rather than a fixed column — which matters, since the two share one strip
/// and the clock is what gives way when they do not both fit.
const DESK_PAD: usize = 4;
/// The clock shows seconds, so this is what makes it tick. The renderer
/// writes only the rows that changed, so a second's repaint is the bar's own
/// rows rather than the screen.
const TICK: Duration = Duration::from_secs(1);

/// The network interface and what is known about it. Not `Copy`, which is why
/// `Readings` no longer is: the interface's NAME is read from a directory and
/// so is owned rather than borrowed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Link {
    pub name: Option<String>,
    pub up: Option<bool>,
    pub address: Address,
}

/// What is known about the interface's address, which is three states and not
/// two. `/proc/net/fib_trie` does not say which INTERFACE a local address
/// belongs to, so "there is no address" and "there is one but it may not be
/// this interface's" are different claims — and printing the second as the
/// first tells an operator their link has no lease while the machine is
/// reachable.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Address {
    /// One local address, and one interface it could belong to.
    Known(String),
    /// The routing table has no non-loopback local address at all.
    Absent,
    /// There is an address, but nothing readable here attributes it.
    #[default]
    Unattributable,
}

/// Everything the bar shows, sampled together so one line cannot mix two
/// moments. Missing readings stay `None` rather than defaulting to zero: a
/// load of 0.00 and a load nobody could read are different claims.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Readings {
    pub load_centi: Option<u64>,
    pub used_kb: Option<u64>,
    pub total_kb: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub epoch_secs: Option<u64>,
    pub link: Link,
}

impl Readings {
    /// Read from a `/proc` and a `/sys` root — parameters so the tests can hand
    /// them fixtures, since the host's own are neither fixed nor reproducible.
    pub fn sample(proc_root: &Path, sys_root: &Path, epoch_secs: Option<u64>) -> Self {
        let loadavg = std::fs::read_to_string(proc_root.join("loadavg")).ok();
        let meminfo = std::fs::read_to_string(proc_root.join("meminfo")).ok();
        let uptime = std::fs::read_to_string(proc_root.join("uptime")).ok();
        let total_kb = meminfo.as_deref().and_then(|text| meminfo_kb(text, "MemTotal"));
        let available_kb = meminfo
            .as_deref()
            .and_then(|text| meminfo_kb(text, "MemAvailable"));
        Self {
            load_centi: loadavg.as_deref().and_then(parse_load_centi),
            // Used is what a person means by "how much is gone", which is
            // total minus AVAILABLE — MemFree counts neither cache nor
            // reclaimable slab and reads alarmingly low on an idle machine.
            used_kb: match (total_kb, available_kb) {
                (Some(total), Some(available)) => Some(total.saturating_sub(available)),
                _ => None,
            },
            total_kb,
            uptime_secs: uptime.as_deref().and_then(parse_uptime_secs),
            epoch_secs,
            link: sample_link(proc_root, sys_root),
        }
    }
}

/// The interface, whether it is up, and its address. Each is answered on its
/// own: a name with no operstate is still worth showing, and an address with
/// no name is not a thing this can produce.
fn sample_link(proc_root: &Path, sys_root: &Path) -> Link {
    let names = interface_names(sys_root);
    let name = choose_interface(&names);
    let up = name.as_deref().and_then(|name| {
        // `operstate` over `carrier`: reading `carrier` on a down interface is
        // an EINVAL rather than a `0`, so the two failure modes would be the
        // same answer here.
        let text = std::fs::read_to_string(sys_root.join("class/net").join(name).join("operstate"))
            .ok()?;
        operational(&text)
    });
    // Only ONE interface makes the address attributable. The dump says which
    // addresses exist and not whose they are, so with a second interface
    // present `NET eth0 <eth1's address>` is a well-formed line that is simply
    // wrong — and a lease on the interface that is NOT named is exactly the
    // case a two-NIC machine hits. Not read at all when nothing was named,
    // since the kernel walks the trie under RCU on every read of that file.
    let address = match name {
        Some(_) if names.len() == 1 => std::fs::read_to_string(proc_root.join("net/fib_trie"))
            .ok()
            .as_deref()
            .map_or(Address::Unattributable, local_ipv4),
        Some(_) => Address::Unattributable,
        None => Address::Unattributable,
    };
    Link { name, up, address }
}

/// `operstate`'s value, and only the two of them that MEAN something. A driver
/// with no carrier reporting writes `unknown`, which is not evidence of a down
/// link — reading it as one would put DOWN beside a working interface, and
/// suppress the address with it. `dormant`, `testing` and `notpresent` are
/// equally not an answer to the question the bar is asking.
fn operational(text: &str) -> Option<bool> {
    match text.trim() {
        "up" => Some(true),
        "down" | "lowerlayerdown" => Some(false),
        _ => None,
    }
}

/// Every interface but loopback, sorted. Non-UTF-8 names are kept LOSSILY
/// rather than dropped, which is what `td-netd` does — dropping one here would
/// be the two crates sorting different lists and naming different interfaces.
fn interface_names(sys_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(sys_root.join("class/net")) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "lo")
        .collect();
    names.sort();
    names
}

/// The interface to report on, by the SAME rule `td-netd` configures one with:
/// prefer a name beginning with `e`, else the first. A second copy of a
/// convention rather than a shared one — the two crates share no library — so
/// a change there is a bar naming an interface nothing configured.
fn choose_interface(names: &[String]) -> Option<String> {
    let ethernet = names.iter().position(|name| name.starts_with('e'));
    match ethernet {
        Some(index) => names.get(index).cloned(),
        None => names.first().cloned(),
    }
}

/// This machine's own IPv4, out of `/proc/net/fib_trie`. Nothing td writes
/// records it — `td-netd` prints the lease and drops it, and `/etc/hosts` gets
/// loopback only — so the kernel's own routing dump is the one file that has
/// it without a `SIOCGIFADDR` ioctl, which would be a new syscall on a surface
/// that has none.
///
/// A local address is a `/32 host LOCAL` leaf, whose value is on the `|--` line
/// above it. Loopback is skipped, and an ambiguous answer — more than one
/// non-loopback local address — is refused rather than guessed at, since the
/// table does not say which interface a leaf belongs to.
fn local_ipv4(text: &str) -> Address {
    let mut previous: Option<&str> = None;
    let mut found: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        // Taken at the TOP of every iteration, so an address is only ever
        // claimed by the line DIRECTLY below it. Carrying it further makes a
        // `/24 link UNICAST` in between invisible, and the leaf below that
        // inherits an address which is not its own.
        let above = previous.take();
        if let Some(address) = trimmed.strip_prefix("|-- ") {
            previous = Some(address.trim());
            continue;
        }
        if !trimmed.starts_with("/32 host LOCAL") {
            continue;
        }
        let Some(address) = above else {
            continue;
        };
        if address.starts_with("127.") {
            continue;
        }
        match found {
            // The same address appears in both the `Main` and `Local` tables,
            // which is agreement rather than ambiguity.
            Some(seen) if seen == address => {}
            Some(_) => return Address::Unattributable,
            None => found = Some(address),
        }
    }
    match found {
        Some(address) => Address::Known(address.to_string()),
        None => Address::Absent,
    }
}

/// `/proc/loadavg`'s first field, as hundredths. Parsed as two integers
/// rather than a float because the shape is fixed and `f64` would make
/// rounding a question nobody needs to answer.
fn parse_load_centi(text: &str) -> Option<u64> {
    let field = text.split_ascii_whitespace().next()?;
    let (whole, fraction) = match field.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (field, "0"),
    };
    let whole: u64 = whole.parse().ok()?;
    // Two digits, PADDED rather than required: the kernel writes `%lu.%02lu`
    // so a shorter one cannot appear here, but reading `1.5` as `1.00` is the
    // wrong answer rather than a refusal, and this is the arithmetic that
    // would carry it.
    let digits = fraction.get(..2).unwrap_or(fraction);
    let hundredths: u64 = match digits.len() {
        0 => 0,
        1 => digits.parse::<u64>().ok()?.checked_mul(10)?,
        _ => digits.parse().ok()?,
    };
    whole.checked_mul(100)?.checked_add(hundredths)
}

/// A named `/proc/meminfo` line's kB value. A line without a colon is
/// SKIPPED rather than ending the search: one malformed line must not hide
/// every field below it.
fn meminfo_kb(text: &str, name: &str) -> Option<u64> {
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        if key != name {
            continue;
        }
        return rest.split_ascii_whitespace().next()?.parse().ok();
    }
    None
}

/// `/proc/uptime`'s first field, whole seconds.
fn parse_uptime_secs(text: &str) -> Option<u64> {
    let field = text.split_ascii_whitespace().next()?;
    let whole = field.split_once('.').map_or(field, |(whole, _)| whole);
    whole.parse().ok()
}

/// Round to one decimal in the largest unit that keeps the number small.
/// Integer arithmetic throughout: a bar is not worth a float.
fn human_bytes(kb: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1024 * 1024 * 1024, "T"),
        (1024 * 1024, "G"),
        (1024, "M"),
    ];
    for (divisor, suffix) in UNITS {
        if kb >= *divisor {
            // The multiply is done in `u128`. `kb.saturating_mul(10)` clamps
            // above `u64::MAX / 10` and the clamp happens BEFORE the divide,
            // so it does not bound the answer — it divides it by ten, and
            // reports a tenth of the truth as though it were a number.
            let tenths = u64::try_from(u128::from(kb).saturating_mul(10) / u128::from(*divisor))
                .unwrap_or(u64::MAX);
            return format!("{}.{}{suffix}", tenths / 10, tenths % 10);
        }
    }
    format!("{kb}K")
}

/// Days, hours and minutes — seconds would churn the line for nothing, and
/// nobody reads an uptime to that resolution.
fn human_uptime(secs: u64) -> String {
    let minutes = secs / 60;
    let (days, rest) = (minutes / (60 * 24), minutes % (60 * 24));
    let (hours, minutes) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}D {hours:02}:{minutes:02}")
    } else {
        format!("{hours:02}:{minutes:02}")
    }
}

/// UTC, spelled out. There is no TZif parser here, so naming the zone is the
/// honest thing: a local-looking time that is silently UTC is worse than a
/// UTC one that says so.
fn utc_stamp(epoch_secs: u64) -> String {
    let days = epoch_secs / 86_400;
    let seconds = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

/// Days since 1970-01-01 to a civil date, by Howard Hinnant's shift-the-era
/// method: March-based years put the leap day last, so the month-length
/// pattern repeats with no table and no branch on February.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days.saturating_add(719_468);
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The line, left to right. A reading that could not be taken shows its
/// label with `?` rather than vanishing, so a bar with a broken source looks
/// broken instead of looking like a machine with less to report.
pub fn line(readings: &Readings) -> String {
    let load = readings.load_centi.map_or_else(
        || "LOAD ?".to_string(),
        |centi| format!("LOAD {}.{:02}", centi / 100, centi % 100),
    );
    let memory = match (readings.used_kb, readings.total_kb) {
        (Some(used), Some(total)) => {
            format!("MEM {}/{}", human_bytes(used), human_bytes(total))
        }
        _ => "MEM ?".to_string(),
    };
    let uptime = readings
        .uptime_secs
        .map_or_else(|| "UP ?".to_string(), |secs| format!("UP {}", human_uptime(secs)));
    let clock = readings
        .epoch_secs
        .map_or_else(|| "CLOCK ?".to_string(), utc_stamp);
    // Leftmost, as the ethernet stanza is in the config this follows, and
    // for the same reason the clock is rightmost.
    let net = match (&readings.link.name, readings.link.up, &readings.link.address) {
        (None, _, _) => "NET ?".to_string(),
        // DOWN outranks any address still configured on the interface: a
        // stale address on a link with no carrier is not somewhere to reach
        // this machine.
        (Some(name), Some(false), _) => format!("NET {name} DOWN"),
        (Some(name), up, address) => {
            // `?` keeps its meaning throughout the bar — could not be
            // determined — so it is the UNATTRIBUTABLE case and not the empty
            // one. A link that is genuinely up with no address is a positive
            // fact and reads as bare `UP`.
            let state = match (address, up) {
                (Address::Known(address), _) => address.clone(),
                (Address::Absent, Some(true)) => "UP".to_string(),
                (Address::Absent, _) => "?".to_string(),
                (Address::Unattributable, Some(true)) => "UP ?".to_string(),
                (Address::Unattributable, _) => "?".to_string(),
            };
            format!("NET {name} {state}")
        }
    };
    [net, load, memory, uptime, clock].join(SEPARATOR)
}

/// The workspaces the strip names: those holding a window, and the ACTIVE one
/// whether or not it holds anything. An operator who switches to an empty
/// workspace would otherwise have nothing on screen saying where they went —
/// the screen is bare either way, and the bar is what tells the two apart.
///
/// Takes the occupied list by value because it is already allocated: the only
/// edit is the active one. Sorted here rather than trusted to arrive that way
/// — it does, `Layout` keeping its workspaces in a `BTreeMap` — because the
/// order this returns is the order they are DRAWN in, and nine numbers cost
/// nothing to sort beside depending on the container type of another module.
/// `spare` is an empty workspace to drag ONTO, named by the layout because the
/// range of workspace numbers is its business rather than the bar's. Merged
/// like the active one — the strip is a set of numbers in order, and where each
/// came from stops mattering once it is in.
pub fn desks(mut occupied: Vec<u8>, active: u8, spare: Option<u8>) -> Vec<u8> {
    occupied.push(active);
    occupied.extend(spare);
    occupied.sort_unstable();
    occupied.dedup();
    occupied
}

/// Each cell as `(number, left, width)`, in the order they are drawn.
///
/// Shared by the painting and the hit test rather than walked twice: a drop
/// lands on the cell an operator aimed at, and two walks that disagreed about
/// where a cell starts would put the window on the workspace NEXT to the one
/// they were pointing at, with the ink saying otherwise.
fn cells(desks: &[u8]) -> impl Iterator<Item = (u8, usize, usize)> + '_ {
    let mut left = 0usize;
    desks.iter().map(move |number| {
        let width = desk_width(*number);
        let at = left;
        left = left.saturating_add(width);
        (*number, at, width)
    })
}

/// The workspace whose cell contains the point, if the strip does at all.
///
/// The bar spans the output's whole width, but only the cells are a workspace:
/// the status line beside them names nothing to drop onto, so a release there
/// answers `None` and is a cancelled drag rather than a move to whichever
/// workspace happens to be last.
pub fn desk_at(desks: &[u8], x: usize, y: usize) -> Option<u8> {
    if y >= BAR_HEIGHT {
        return None;
    }
    // A loop rather than the iterator method that would read more naturally:
    // the bootstrap ladder's guard scans this source for that name and cannot
    // tell an iterator from GNU findutils.
    for (number, left, width) in cells(desks) {
        if x >= left && x < left.saturating_add(width) {
            return Some(number);
        }
    }
    None
}

/// The cell a workspace is drawn in, for a caller that has to draw something
/// else over it.
pub fn desk_cell(desks: &[u8], number: u8) -> Option<(usize, usize)> {
    for (candidate, left, width) in cells(desks) {
        if candidate == number {
            return Some((left, width));
        }
    }
    None
}

/// `number` in decimal, written into a caller's buffer. The compositor draws
/// this every frame, so it is a stack buffer and not a `String`: three digits
/// bound a `u8`, and both the label and the width a cell needs are measured
/// through here, which is what stops the two disagreeing about a two-digit
/// number.
///
/// The bytes are ASCII by construction, so the `from_utf8` cannot fail; the
/// empty string is what an impossible failure draws, since this crate does not
/// unwrap.
fn decimal(number: u8, buffer: &mut [u8; 3]) -> &str {
    let mut at = buffer.len();
    let mut left = number;
    loop {
        at = at.saturating_sub(1);
        if let Some(slot) = buffer.get_mut(at) {
            *slot = b'0'.saturating_add(left % 10);
        }
        left /= 10;
        if left == 0 || at == 0 {
            break;
        }
    }
    buffer
        .get(at..)
        .and_then(|of| std::str::from_utf8(of).ok())
        .unwrap_or("")
}

/// How wide `number` draws in the strip, cell air included.
fn desk_width(number: u8) -> usize {
    let mut buffer = [0u8; 3];
    decimal(number, &mut buffer)
        .len()
        .saturating_mul(ui::GLYPH_ADVANCE)
        .saturating_mul(SCALE)
        .saturating_add(DESK_PAD.saturating_mul(2))
}

/// How far into its cell a label starts, so the INK lands centred. Padding
/// both sides by `DESK_PAD` does not: a glyph's ADVANCE carries a trailing
/// column the glyph never fills, so the number would sit a pixel left of
/// centre — which is invisible on the strip and not inside the active cell,
/// where a solid block surrounds it.
fn label_inset(cell_width: usize, label: &str) -> usize {
    let ink = label
        .len()
        .saturating_mul(ui::GLYPH_ADVANCE)
        .saturating_sub(ui::GLYPH_ADVANCE.saturating_sub(ui::GLYPH_WIDTH))
        .saturating_mul(SCALE);
    cell_width.saturating_sub(ink) / 2
}

/// Paint the strip across the top. The caller owns both the workspaces and
/// the text, so this never reads a clock, a file, or a layout — the same
/// split the launcher and the sheet have.
///
/// The workspaces are LEFTMOST and the status line follows them, which is
/// where an operator looks for them and is also the order that survives a
/// narrow output: the strip is clipped from the right, so what gives way
/// first is the end of the status line rather than where the operator is.
pub fn paint(
    frame: &mut [u8],
    width: usize,
    height: usize,
    stride: usize,
    desks: &[u8],
    active: u8,
    text: &str,
) {
    // Not clamped to `height`: both primitives clip against it, and a second
    // clamp here would read as though they did not.
    let bar = (0, 0, width, BAR_HEIGHT);
    ui::fill(frame, width, height, stride, bar, BACKGROUND);
    let mut left = 0usize;
    let mut label = [0u8; 3];
    for (number, at, cell_width) in cells(desks) {
        let cell = (at, 0, cell_width, BAR_HEIGHT);
        // The active workspace is the strip's own two colours EXCHANGED. It
        // needs no third colour and no glyph beside the number, and inverse
        // video says "you are here" without the operator being told which of
        // two shades of one hue means what.
        let ink = if number == active {
            ui::fill(frame, width, height, stride, cell, INK);
            BACKGROUND
        } else {
            INK
        };
        let written = decimal(number, &mut label);
        ui::draw_text_clipped(
            frame,
            width,
            height,
            stride,
            at.saturating_add(label_inset(cell.2, written)),
            TEXT_TOP,
            SCALE,
            written,
            ink,
            cell,
        );
        left = at.saturating_add(cell.2);
    }
    // What keeps the status line off the cells TODAY is where it starts: a
    // line is drawn rightwards from there and cannot reach back. The clip
    // rectangle is what keeps that true of a line positioned any other way —
    // right-aligned, say — and it costs nothing to narrow, since a narrower
    // clip can only ever remove drawing.
    let text_left = left.saturating_add(TEXT_LEFT);
    ui::draw_text_clipped(
        frame,
        width,
        height,
        stride,
        text_left,
        TEXT_TOP,
        SCALE,
        text,
        INK,
        (text_left, 0, width.saturating_sub(text_left), BAR_HEIGHT),
    );
}

/// At most this many failures are named between good paints. Deduplication
/// alone bounds the REPEATED fault and not the alternating one: a framebuffer
/// failing every other paint, or two faults taking turns, is a fresh error
/// every tick to a record that remembers only the last.
const MAX_REPORTS: usize = 4;

/// One line per DISTINCT failure rather than one per tick, and a hard stop
/// after `MAX_REPORTS` of them. The retry is not optional — `set_status` puts
/// the previous line back when a paint fails, so the next second's text
/// differs and the paint is attempted again — and an output broken for good
/// would otherwise write a line a second forever, burying whatever else the
/// session had to say.
#[derive(Default)]
struct Reported {
    last: Option<String>,
    since_good: usize,
}

impl Reported {
    /// What to print for this outcome, if anything. A paint that worked clears
    /// the record, so a fault that returns after the bar recovered is named
    /// again rather than swallowed for the life of the session.
    fn note(&mut self, error: Option<&str>) -> Option<String> {
        let Some(error) = error else {
            self.last = None;
            self.since_good = 0;
            return None;
        };
        if self.last.as_deref() == Some(error) {
            return None;
        }
        self.last = Some(error.to_string());
        self.since_good = self.since_good.saturating_add(1);
        if self.since_good < MAX_REPORTS {
            Some(error.to_string())
        } else if self.since_good == MAX_REPORTS {
            Some(format!("{error} (silent until a paint succeeds)"))
        } else {
            None
        }
    }
}

/// What one sample decided. `Poisoned` means another thread panicked holding
/// the runtime: nothing this one does can help, and looping on it would spin a
/// core printing.
#[derive(Debug, Eq, PartialEq)]
enum Tick {
    Continue(Option<String>),
    Poisoned,
}

/// One sample published. Split out of the loop so the WIRING is reachable from
/// a test — the record, the report, and releasing the lock before printing are
/// each things a refactor can quietly drop while `Reported`'s own tests stay
/// green. `epoch_secs` is a parameter for the same reason the `/proc` root is.
fn tick(
    runtime: &Mutex<crate::runtime::Runtime>,
    proc_root: &Path,
    sys_root: &Path,
    epoch_secs: Option<u64>,
    reported: &mut Reported,
) -> Tick {
    let readings = Readings::sample(proc_root, sys_root, epoch_secs);
    let line = line(&readings);
    // The guard is released BEFORE anything is printed. `eprintln!` takes
    // stderr's own lock and blocks on a slow console, and holding the runtime
    // across that would stop the compositor over the least important thing on
    // the screen — the opposite of what the reporting is for.
    let outcome = match runtime.lock() {
        Ok(mut runtime) => runtime.set_status(line),
        Err(_) => return Tick::Poisoned,
    };
    Tick::Continue(reported.note(outcome.err().as_deref()))
}

/// Sample and publish on a cadence. A compositor with no timer repaints only
/// on input, so a clock would sit at the moment the machine booted; this is
/// the only thing in the process that wakes without one.
pub fn start(
    runtime: Arc<Mutex<crate::runtime::Runtime>>,
    proc_root: PathBuf,
    sys_root: PathBuf,
) -> Result<(), String> {
    // `thread::Builder`, not `thread::spawn`: the latter PANICS when the OS
    // refuses a thread, which would take the session down over the least
    // important thing on the screen.
    thread::Builder::new()
        .name("td-status-bar".to_string())
        .spawn(move || {
            let mut reported = Reported::default();
            loop {
                // A paint failure is REPORTED, never fatal: the bar is the
                // least important thing on the screen and must not take the
                // session down with it.
                match tick(&runtime, &proc_root, &sys_root, unix_epoch_secs(), &mut reported) {
                    Tick::Continue(report) => {
                        if let Some(report) = report {
                            eprintln!("td-compositor: status bar: {report}");
                        }
                    }
                    Tick::Poisoned => {
                        eprintln!("td-compositor: status bar: runtime poisoned, clock stopped");
                        return;
                    }
                }
                thread::sleep(TICK);
            }
        })
        .map(|_| ())
        .map_err(|error| format!("spawn status bar thread: {error}"))
}

/// Wall clock as UNIX seconds, or `None` before the epoch — which a machine
/// with no RTC and no network can genuinely report.
fn unix_epoch_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        proc_root: std::path::PathBuf,
        sys_root: std::path::PathBuf,
    }

    impl Fixture {
        /// No interfaces and no routing table, so the network field reads as
        /// unanswerable — which is what every test that predates it expects.
        fn new(loadavg: &str, meminfo: &str, uptime: &str) -> Self {
            Self::with_net(loadavg, meminfo, uptime, &[], "")
        }

        fn with_net(
            loadavg: &str,
            meminfo: &str,
            uptime: &str,
            interfaces: &[(&str, &str)],
            fib_trie: &str,
        ) -> Self {
            let stem = format!(
                "td-bar-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            let proc_root = std::env::temp_dir().join(format!("{stem}-proc"));
            let sys_root = std::env::temp_dir().join(format!("{stem}-sys"));
            std::fs::create_dir_all(&proc_root).unwrap();
            std::fs::write(proc_root.join("loadavg"), loadavg).unwrap();
            std::fs::write(proc_root.join("meminfo"), meminfo).unwrap();
            std::fs::write(proc_root.join("uptime"), uptime).unwrap();
            if !fib_trie.is_empty() {
                std::fs::create_dir_all(proc_root.join("net")).unwrap();
                std::fs::write(proc_root.join("net").join("fib_trie"), fib_trie).unwrap();
            }
            for (name, operstate) in interfaces {
                let dir = sys_root.join("class").join("net").join(name);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("operstate"), operstate).unwrap();
            }
            Self {
                proc_root,
                sys_root,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.proc_root);
            let _ = std::fs::remove_dir_all(&self.sys_root);
        }
    }

    /// The shape the kernel writes: every local address is a `/32 host LOCAL`
    /// leaf under the `|--` line carrying it, and the `Main` and `Local`
    /// tables both list it.
    const FIB_TRIE: &str = "\
Main:
  +-- 0.0.0.0/0 3 0 5
     |-- 10.0.2.0
        /24 link UNICAST
     |-- 10.0.2.15
        /32 host LOCAL
     |-- 127.0.0.0
        /8 host LOCAL
Local:
  +-- 0.0.0.0/0 3 0 5
     |-- 10.0.2.15
        /32 host LOCAL
     |-- 127.0.0.1
        /32 host LOCAL
";

    const MEMINFO: &str = "MemTotal:        8039384 kB\n\
                           MemFree:          204512 kB\n\
                           MemAvailable:    5242880 kB\n\
                           Buffers:          131072 kB\n";

    #[test]
    fn a_sample_reads_every_field_from_its_proc_root() {
        let fixture = Fixture::new("0.42 0.31 0.28 2/517 9182\n", MEMINFO, "187245.31 91.2\n");
        let readings = Readings::sample(&fixture.proc_root, &fixture.sys_root, Some(1_770_000_000));
        assert_eq!(readings.load_centi, Some(42));
        assert_eq!(readings.total_kb, Some(8_039_384));
        // Used is total minus AVAILABLE, not minus free.
        assert_eq!(readings.used_kb, Some(8_039_384 - 5_242_880));
        assert_eq!(readings.uptime_secs, Some(187_245));
        assert_eq!(
            line(&readings),
            "NET ?  LOAD 0.42  MEM 2.6G/7.6G  UP 2D 04:00  2026-02-02 02:40:00 UTC"
        );
    }

    #[test]
    fn the_network_field_names_the_interface_and_its_address() {
        let fixture = Fixture::with_net(
            "0.42 0.31 0.28 2/517 9182\n",
            MEMINFO,
            "187245.31 91.2\n",
            &[("eth0", "up\n"), ("lo", "unknown\n")],
            FIB_TRIE,
        );
        let readings = Readings::sample(&fixture.proc_root, &fixture.sys_root, Some(0));
        assert_eq!(readings.link.name.as_deref(), Some("eth0"));
        assert_eq!(readings.link.up, Some(true));
        assert_eq!(readings.link.address, Address::Known("10.0.2.15".to_string()));
        assert!(line(&readings).starts_with("NET eth0 10.0.2.15  LOAD"));
    }

    #[test]
    fn a_down_interface_says_so_and_an_up_one_with_no_lease_says_that() {
        let down = Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[("eth0", "down\n")], "");
        let readings = Readings::sample(&down.proc_root, &down.sys_root, Some(0));
        assert_eq!(readings.link.up, Some(false));
        assert!(line(&readings).starts_with("NET eth0 DOWN"));

        // Up with no address is a REAL state — a link with no lease — and it
        // is not the same claim as an interface nobody could name. The table
        // is PRESENT and holds only loopback, which is what makes this
        // "there is none" rather than "could not tell".
        let leaseless = Fixture::with_net(
            "0 0 0\n",
            MEMINFO,
            "0\n",
            &[("eth0", "up\n")],
            "  |-- 127.0.0.1\n     /32 host LOCAL\n",
        );
        let readings = Readings::sample(&leaseless.proc_root, &leaseless.sys_root, Some(0));
        assert_eq!(readings.link.address, Address::Absent);
        assert!(line(&readings).starts_with("NET eth0 UP  LOAD"));

        // No routing table at all is neither of those: it is the question
        // going unanswered, and `?` is what the rest of this bar spells that
        // with.
        let blind = Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[("eth0", "up\n")], "");
        let readings = Readings::sample(&blind.proc_root, &blind.sys_root, Some(0));
        assert_eq!(readings.link.address, Address::Unattributable);
        assert!(line(&readings).starts_with("NET eth0 UP ?  LOAD"));
    }

    #[test]
    fn a_second_interface_makes_the_address_unattributable() {
        // The dump says an address EXISTS, never whose it is. With two
        // interfaces the named one may not be the one holding the lease, and
        // `NET eth0 <eth1's address>` is a well-formed line that is wrong —
        // which for a status bar is worse than showing less.
        let two = Fixture::with_net(
            "0 0 0\n",
            MEMINFO,
            "0\n",
            &[("eth0", "up\n"), ("eth1", "up\n"), ("lo", "unknown\n")],
            FIB_TRIE,
        );
        let readings = Readings::sample(&two.proc_root, &two.sys_root, Some(0));
        assert_eq!(readings.link.name.as_deref(), Some("eth0"));
        assert_eq!(readings.link.address, Address::Unattributable);
        assert!(line(&readings).starts_with("NET eth0 UP ?  LOAD"));

        // The very same routing table IS attributable once there is only one
        // interface it could belong to.
        let one = Fixture::with_net(
            "0 0 0\n",
            MEMINFO,
            "0\n",
            &[("eth0", "up\n"), ("lo", "unknown\n")],
            FIB_TRIE,
        );
        let readings = Readings::sample(&one.proc_root, &one.sys_root, Some(0));
        assert_eq!(
            readings.link.address,
            Address::Known("10.0.2.15".to_string())
        );
    }

    #[test]
    fn an_operstate_that_is_not_an_answer_is_not_read_as_down() {
        // `unknown` is what a driver with no carrier reporting writes. Reading
        // it as DOWN would put that beside a working interface and hide its
        // address with it.
        for state in ["unknown", "dormant", "testing", "notpresent", "nonsense"] {
            assert_eq!(operational(state), None, "{state}");
        }
        assert_eq!(operational("up\n"), Some(true));
        assert_eq!(operational("down\n"), Some(false));
        assert_eq!(operational("lowerlayerdown\n"), Some(false));

        let unknown =
            Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[("eth0", "unknown\n")], FIB_TRIE);
        let readings = Readings::sample(&unknown.proc_root, &unknown.sys_root, Some(0));
        assert_eq!(readings.link.up, None);
        // The address still shows: an unknown link state is no reason to
        // withhold an address the routing table has.
        assert!(line(&readings).starts_with("NET eth0 10.0.2.15  LOAD"));

        // Named, but with no readable state and no address at all: `?`.
        let stateless = Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[], "");
        std::fs::create_dir_all(stateless.sys_root.join("class").join("net").join("eth0")).unwrap();
        let readings = Readings::sample(&stateless.proc_root, &stateless.sys_root, Some(0));
        assert_eq!(readings.link.name.as_deref(), Some("eth0"));
        assert_eq!(readings.link.up, None);
        assert!(line(&readings).starts_with("NET eth0 ?  LOAD"));
    }

    #[test]
    fn a_down_link_reports_down_rather_than_the_address_it_still_holds() {
        // A stale address on a link with no carrier is not somewhere to reach
        // this machine, so DOWN outranks it. Deliberate, and it would not be
        // held by any other test here.
        let down = Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[("eth0", "down\n")], FIB_TRIE);
        let readings = Readings::sample(&down.proc_root, &down.sys_root, Some(0));
        assert_eq!(
            readings.link.address,
            Address::Known("10.0.2.15".to_string())
        );
        assert!(line(&readings).starts_with("NET eth0 DOWN  LOAD"));
    }

    #[test]
    fn the_interface_is_chosen_as_td_netd_chooses_one() {
        // Loopback is never it, a name beginning with `e` wins over one that
        // does not whatever the sort order, and the sort is what decides
        // between two of the same kind.
        let fixture = Fixture::with_net(
            "0 0 0\n",
            MEMINFO,
            "0\n",
            // `ap0` sorts BEFORE `eth0`, so picking the first name would
            // answer `ap0` — which is what makes this about the `e` rule
            // rather than about the sort.
            &[
                ("lo", "unknown\n"),
                ("wlan0", "up\n"),
                ("eth1", "up\n"),
                ("ap0", "up\n"),
                ("eth0", "up\n"),
            ],
            "",
        );
        assert_eq!(
            choose_interface(&interface_names(&fixture.sys_root)).as_deref(),
            Some("eth0")
        );

        let no_ethernet = Fixture::with_net(
            "0 0 0\n",
            MEMINFO,
            "0\n",
            &[("lo", "unknown\n"), ("wlan0", "up\n")],
            "",
        );
        assert_eq!(
            choose_interface(&interface_names(&no_ethernet.sys_root)).as_deref(),
            Some("wlan0")
        );

        let only_loopback =
            Fixture::with_net("0 0 0\n", MEMINFO, "0\n", &[("lo", "unknown\n")], "");
        assert_eq!(choose_interface(&interface_names(&only_loopback.sys_root)), None);
    }

    #[test]
    fn the_routing_table_gives_one_local_address_or_none() {
        assert_eq!(local_ipv4(FIB_TRIE), Address::Known("10.0.2.15".to_string()));
        // Loopback is not this machine's address in any useful sense, and it
        // is present in every routing table, so it must not be the answer.
        assert_eq!(
            local_ipv4("  |-- 127.0.0.1\n     /32 host LOCAL\n"),
            Address::Absent
        );
        // Two different non-loopback locals: the table does not say which
        // interface owns which, so guessing would be showing the wrong one.
        let two = "  |-- 10.0.2.15\n     /32 host LOCAL\n  |-- 192.168.1.4\n     /32 host LOCAL\n";
        assert_eq!(local_ipv4(two), Address::Unattributable);
        // A LOCAL leaf must not inherit the address of a line it is not
        // directly below. Here the `/24` in between is what a real dump has,
        // and without clearing the carry the malformed leaf below it would be
        // reported as 10.0.2.0 — a network address printed as this machine's.
        let stale = "  |-- 10.0.2.0\n     /24 link UNICAST\n     /32 host LOCAL\n";
        assert_eq!(local_ipv4(stale), Address::Absent);
        // The prefix LENGTH is part of the pattern, not decoration: a
        // non-loopback `/8 host LOCAL` is not a host address.
        assert_eq!(
            local_ipv4("  |-- 10.0.0.0\n     /8 host LOCAL\n"),
            Address::Absent
        );
        // A `/32` that is not LOCAL is a route, not an address; and a LOCAL
        // leaf with no address line above it is a malformed dump, not a hit.
        assert_eq!(
            local_ipv4("  |-- 10.0.2.15\n     /32 host UNICAST\n"),
            Address::Absent
        );
        assert_eq!(local_ipv4("     /32 host LOCAL\n"), Address::Absent);
        assert_eq!(local_ipv4(""), Address::Absent);
    }

    #[test]
    fn a_missing_or_malformed_source_shows_a_question_mark_not_a_zero() {
        let empty = std::env::temp_dir().join(format!(
            "td-bar-absent-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let readings = Readings::sample(&empty, &empty, None);
        assert_eq!(readings, Readings::default());
        assert_eq!(line(&readings), "NET ?  LOAD ?  MEM ?  UP ?  CLOCK ?");

        // Present but unparseable is the same answer, and each field fails on
        // its own: a garbled loadavg must not take the clock down with it.
        let fixture = Fixture::new("not-a-number\n", "MemTotal: elephants\n", "\n");
        let readings = Readings::sample(&fixture.proc_root, &fixture.sys_root, Some(0));
        assert_eq!(readings.load_centi, None);
        assert_eq!(readings.total_kb, None);
        assert_eq!(readings.used_kb, None);
        assert_eq!(readings.uptime_secs, None);
        assert_eq!(
            line(&readings),
            "NET ?  LOAD ?  MEM ?  UP ?  1970-01-01 00:00:00 UTC"
        );
    }

    #[test]
    fn memory_needs_both_halves_before_it_reports_either() {
        // MemAvailable absent: "used" is unanswerable, so the whole segment
        // is, rather than reporting a total beside a made-up used.
        let fixture = Fixture::new("0.00 0 0 1/1 1\n", "MemTotal:        1048576 kB\n", "0\n");
        let readings = Readings::sample(&fixture.proc_root, &fixture.sys_root, Some(0));
        assert_eq!(readings.total_kb, Some(1_048_576));
        assert_eq!(readings.used_kb, None);
        assert!(line(&readings).contains("MEM ?"));
    }

    #[test]
    fn load_parses_the_first_field_at_two_decimals() {
        assert_eq!(parse_load_centi("0.42 0.31 0.28 2/517 9182"), Some(42));
        assert_eq!(parse_load_centi("12.07 0.31 0.28"), Some(1207));
        // Three decimals is not this file's format, but truncating beats
        // refusing: the extra digit changes nothing a bar shows.
        assert_eq!(parse_load_centi("1.234"), Some(123));
        // One decimal is not this file's format either, and padding it is
        // the right answer where truncating to `1.00` is a wrong one.
        assert_eq!(parse_load_centi("1.5"), Some(150));
        assert_eq!(parse_load_centi("2."), Some(200));
        assert_eq!(parse_load_centi("7 1 1"), Some(700));
        assert_eq!(parse_load_centi(""), None);
        assert_eq!(parse_load_centi("x.yz"), None);
    }

    #[test]
    fn meminfo_matches_a_whole_key_and_not_a_prefix() {
        // `MemTotal` must not be answered by `MemTotalSwap`, and the search
        // must reach a key that is not the first line.
        let text = "MemTotalSwap:   1 kB\nMemFree:  2 kB\nMemTotal:   4096 kB\n";
        assert_eq!(meminfo_kb(text, "MemTotal"), Some(4096));
        assert_eq!(meminfo_kb(text, "MemFree"), Some(2));
        assert_eq!(meminfo_kb(text, "MemAvailable"), None);
        // A line with no colon must not hide the fields below it.
        let ragged = "MemTotal:  4096 kB\nnonsense\nMemAvailable:  2048 kB\n";
        assert_eq!(meminfo_kb(ragged, "MemAvailable"), Some(2048));
    }

    #[test]
    fn bytes_and_uptime_render_in_the_units_a_person_reads() {
        assert_eq!(human_bytes(0), "0K");
        assert_eq!(human_bytes(512), "512K");
        assert_eq!(human_bytes(1024), "1.0M");
        assert_eq!(human_bytes(1536), "1.5M");
        assert_eq!(human_bytes(8_039_384), "7.6G");
        assert_eq!(human_bytes(1024 * 1024 * 1024), "1.0T");
        // Recomputed independently rather than read off the implementation:
        // u64::MAX kB IS 17179869183.9T, and a `u64` tenths multiply used to
        // print a tenth of that with no sign that anything had clamped.
        assert_eq!(human_bytes(u64::MAX), "17179869183.9T");

        assert_eq!(human_uptime(0), "00:00");
        assert_eq!(human_uptime(59), "00:00");
        assert_eq!(human_uptime(60), "00:01");
        assert_eq!(human_uptime(3600), "01:00");
        assert_eq!(human_uptime(86_400), "1D 00:00");
        assert_eq!(human_uptime(187_245), "2D 04:00");
    }

    #[test]
    fn the_civil_calendar_holds_across_leap_years_and_centuries() {
        assert_eq!(utc_stamp(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc_stamp(86_399), "1970-01-01 23:59:59 UTC");
        assert_eq!(utc_stamp(86_400), "1970-01-02 00:00:00 UTC");
        // 1972 is a leap year and 2000 is (the 400 rule); 2100 will not be
        // (the 100 rule), which is the one a naive every-fourth-year gets
        // wrong. 1900 is the other side of that rule and cannot be asked
        // here — it is before the epoch, and these are unsigned days since
        // it.
        assert_eq!(utc_stamp(68_255_999), "1972-02-29 23:59:59 UTC");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(utc_stamp(4_107_542_400), "2100-03-01 00:00:00 UTC");
        assert_eq!(utc_stamp(4_107_456_000), "2100-02-28 00:00:00 UTC");
        assert_eq!(utc_stamp(1_770_000_000), "2026-02-02 02:40:00 UTC");
        // Every day of a leap year and the year after it, walked in order.
        // The round trip alone would pass against a matching pair of wrong
        // functions, so each step is also required to ADVANCE the calendar
        // by exactly one day — which only `civil_from_days` can be wrong
        // about.
        for (year, length) in [(2024u64, 366u64), (2025, 365)] {
            let start = days_from_civil(year, 1, 1);
            let mut previous = civil_from_days(start);
            assert_eq!(previous, (year, 1, 1));
            for offset in 1..length {
                let today = civil_from_days(start + offset);
                assert_eq!(days_from_civil(today.0, today.1, today.2), start + offset);
                assert_eq!(next_day(previous), today, "after {previous:?}");
                previous = today;
            }
            assert_eq!(next_day(previous), (year + 1, 1, 1), "after {previous:?}");
        }
    }

    /// The next date, written out as month lengths and the leap rule rather
    /// than as arithmetic on a day count — so a walk through the year checks
    /// `civil_from_days` against a different method rather than against its
    /// own inverse, which a matching pair of wrong functions would satisfy.
    fn next_day((year, month, day): (u64, u64, u64)) -> (u64, u64, u64) {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let length = match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            _ => 28,
        };
        if day < length {
            (year, month, day + 1)
        } else if month < 12 {
            (year, month + 1, 1)
        } else {
            (year + 1, 1, 1)
        }
    }

    #[test]
    fn a_workspace_number_writes_itself_into_a_borrowed_buffer() {
        // The whole `u8` range, not just the nine workspaces there are: the
        // cell's WIDTH is this string's length, so a number that wrote more
        // digits than it measured would draw its own label over its neighbour.
        for number in 0..=u8::MAX {
            let mut buffer = [0u8; 3];
            let written = decimal(number, &mut buffer).to_string();
            assert_eq!(written, format!("{number}"), "{number} is written wrong");
            assert_eq!(
                desk_width(number),
                written.len() * ui::GLYPH_ADVANCE * SCALE + DESK_PAD * 2
            );
        }
        // One buffer serves every cell in a paint, so a SHORTER number after a
        // longer one must not carry the longer one's leading digits.
        let mut buffer = [0u8; 3];
        assert_eq!(decimal(255, &mut buffer), "255");
        assert_eq!(decimal(7, &mut buffer), "7");
    }

    #[test]
    fn the_strip_names_every_occupied_workspace_and_the_one_being_looked_at() {
        // An EMPTY active workspace is the case this exists for: switching to
        // one leaves a bare screen, and without its number on the strip the
        // operator has no way to tell that from the workspace they left.
        assert_eq!(desks(Vec::new(), 4, None), [4]);
        assert_eq!(desks(vec![1, 3], 4, None), [1, 3, 4]);
        // Sorted whatever the active one is, and named ONCE when it holds
        // windows of its own — it is in both lists and is one place on screen.
        assert_eq!(desks(vec![2, 9], 5, None), [2, 5, 9]);
        assert_eq!(desks(vec![1, 2, 3], 2, None), [1, 2, 3]);
    }

    #[test]
    fn the_strip_carries_a_spare_workspace_to_drag_onto() {
        // The ordinary machine: one workspace in use, and a second named so
        // there is somewhere to drop a window that is not where it already is.
        assert_eq!(desks(vec![1], 1, Some(2)), [1, 2]);
        // Merged in ORDER rather than appended: a spare filling a gap belongs
        // where its number puts it, or the strip would read 1 3 2.
        assert_eq!(desks(vec![1, 3], 1, Some(2)), [1, 2, 3]);
        // Deduplicated like the active one, so a spare that somehow names a
        // workspace already on the strip is still one cell.
        assert_eq!(desks(vec![1, 2], 1, Some(2)), [1, 2]);
        // No spare is the strip as it was: nine workspaces deep, there is none
        // to offer and the bar says so by not growing.
        assert_eq!(desks(vec![1, 2], 1, None), [1, 2]);
    }

    #[test]
    fn a_point_on_the_strip_names_the_workspace_under_it() {
        let desks = [1u8, 2, 3];
        // Every cell answers over its own span, and the walk that places them
        // is the one `paint` draws with.
        let mut left = 0usize;
        for number in desks {
            let width = desk_width(number);
            assert_eq!(desk_at(&desks, left, 0), Some(number), "cell {number} left");
            assert_eq!(
                desk_at(&desks, left + width - 1, BAR_HEIGHT - 1),
                Some(number),
                "cell {number} right"
            );
            assert_eq!(desk_cell(&desks, number), Some((left, width)));
            left += width;
        }
        // Past the last cell is the status line, which names no workspace: a
        // release there is a cancelled drag rather than a move to whichever
        // number happened to be drawn last.
        assert_eq!(desk_at(&desks, left, 0), None);
        // Below the bar is a window, however far right or left.
        assert_eq!(desk_at(&desks, 0, BAR_HEIGHT), None);
        assert_eq!(desk_cell(&desks, 9), None);
    }

    #[test]
    fn the_active_workspace_is_the_strips_own_colours_exchanged() {
        let (width, height) = (400usize, BAR_HEIGHT);
        let stride = width * 4;
        // The SAME cell either way round. Comparing two different workspaces
        // would compare two different glyphs, which are not each other's
        // inverse and never could be.
        let cell_one = |active: u8| {
            let mut frame = vec![0u8; stride * height];
            paint(&mut frame, width, height, stride, &[1, 2], active, "");
            let (mut ink, mut background) = (0usize, 0usize);
            for y in 0..BAR_HEIGHT {
                for x in 0..desk_width(1) {
                    match frame.get(y * stride + x * 4..y * stride + x * 4 + 4) {
                        Some(pixel) if pixel == INK => ink += 1,
                        Some(pixel) if pixel == BACKGROUND => background += 1,
                        _ => {}
                    }
                }
            }
            (ink, background)
        };
        // Idle, the cell is a number on the strip: mostly background, some
        // ink. Active, it is those two counts SWAPPED — a block of ink with
        // the number cut out of it — which is what makes "you are here"
        // readable without a third colour or a glyph beside the number.
        let idle = cell_one(2);
        let live = cell_one(1);
        assert!(
            idle.0 > 0 && idle.1 > idle.0,
            "1 is not a number on the strip"
        );
        assert!(
            live.1 > 0 && live.0 > live.1,
            "the active cell is not a block"
        );
        assert_eq!(
            idle,
            (live.1, live.0),
            "the cell is not one picture and its inverse"
        );
    }

    #[test]
    fn the_status_line_starts_after_the_workspaces_and_is_cut_before_reaching_them() {
        let (width, height) = (400usize, BAR_HEIGHT);
        let stride = width * 4;
        let ink_left = |desks: &[u8], text: &str| {
            let mut frame = vec![0u8; stride * height];
            paint(&mut frame, width, height, stride, desks, 0, text);
            // `position`, not the iterator method whose name is also a shell
            // command: this file is `include_str!`'d into the td-compositor
            // recipe, so its text is scanned as a bootstrap step's. Over
            // `0..width` the index it answers IS the column.
            (0..width).position(|x| {
                (0..BAR_HEIGHT).any(|y| {
                    frame.get(y * stride + x * 4..y * stride + x * 4 + 4) == Some(&INK[..])
                })
            })
        };
        // Active 0 is no workspace, so every cell here is a plain number and
        // the leftmost ink is the FIRST one — which is what says the strip
        // starts with the workspaces rather than with the status line.
        assert_eq!(ink_left(&[], "LOAD 0.42"), Some(TEXT_LEFT));
        assert_eq!(
            ink_left(&[7], "LOAD 0.42"),
            Some(label_inset(desk_width(7), "7"))
        );

        // The number is CENTRED in its cell, air equal either side. Padding
        // both sides by the same number does not centre it, since a glyph's
        // advance carries a column it never inks — and off-centre is what an
        // inverted cell, a block of ink around the number, makes visible.
        let mut alone = vec![0u8; stride * height];
        paint(&mut alone, width, height, stride, &[7], 0, "");
        let inked: Vec<usize> = (0..desk_width(7))
            .filter(|x| {
                (0..BAR_HEIGHT).any(|y| {
                    alone.get(y * stride + x * 4..y * stride + x * 4 + 4) == Some(&INK[..])
                })
            })
            .collect();
        let (first, last) = (inked.first().copied(), inked.last().copied());
        assert_eq!(
            first,
            last.map(|last| desk_width(7).saturating_sub(last).saturating_sub(1)),
            "the number is not centred in its cell: {first:?}..{last:?} of {}",
            desk_width(7)
        );

        // And the status is clipped to what they leave rather than drawn over
        // them: a line long enough to reach a cell loses its own end, not the
        // one thing on the strip that says where the operator is. Asserted as
        // the cell's pixels being exactly what NO status line leaves, for the
        // cell both ways round. Idle is the sensitive one — ink landing on
        // background is a change at every pixel it touches — while inside the
        // ACTIVE cell most of an overdraw is ink on ink and only the digit
        // could show it, which is why one of the two would not do.
        let narrow = 64usize;
        let cell_pixels = |active: u8, text: &str| {
            let mut frame = vec![0u8; stride * height];
            paint(&mut frame, narrow, height, stride, &[1], active, text);
            let mut pixels = Vec::new();
            for y in 0..BAR_HEIGHT {
                for x in 0..desk_width(1) {
                    pixels.push(
                        frame
                            .get(y * stride + x * 4..y * stride + x * 4 + 4)
                            .map(<[u8]>::to_vec),
                    );
                }
            }
            pixels
        };
        for active in [0u8, 1] {
            assert_eq!(
                cell_pixels(active, ""),
                cell_pixels(active, &"X".repeat(narrow)),
                "the status line reached the workspace cell (active {active})"
            );
        }
    }

    #[test]
    fn a_repeated_failure_is_reported_once_and_a_returning_one_again() {
        let mut reported = Reported::default();
        assert_eq!(reported.note(Some("no framebuffer")).as_deref(), Some("no framebuffer"));
        assert_eq!(reported.note(Some("no framebuffer")), None);
        assert_eq!(reported.note(Some("no framebuffer")), None);
        // A different fault is news even while the first is unresolved.
        assert_eq!(reported.note(Some("short write")).as_deref(), Some("short write"));
        assert_eq!(reported.note(Some("short write")), None);
        assert_eq!(reported.note(None), None);
        assert_eq!(reported.note(Some("short write")).as_deref(), Some("short write"));
    }

    #[test]
    fn alternating_failures_go_quiet_rather_than_reporting_forever() {
        // Deduplication alone does not bound this: each of these is a fresh
        // error to a record that remembers only the last, so without the cap
        // it is a line per tick for as long as the fault flaps.
        let mut reported = Reported::default();
        let mut lines = 0usize;
        for turn in 0..50 {
            let error = if turn % 2 == 0 { "odd write" } else { "even write" };
            if reported.note(Some(error)).is_some() {
                lines = lines.saturating_add(1);
            }
        }
        assert_eq!(lines, MAX_REPORTS, "{lines} lines for one flapping fault");
        // And a good paint re-arms it, so a fault that flaps again later is
        // not silent for the life of the session.
        assert_eq!(reported.note(None), None);
        assert!(reported.note(Some("odd write")).is_some());
    }

    #[test]
    fn the_sampler_reports_a_failure_once_and_names_it_again_after_a_good_paint() {
        // The WIRING, not the record: deleting the reset or the dedupe inside
        // `tick` leaves `Reported`'s own tests green, because nothing else
        // calls it.
        let path = std::env::temp_dir().join(format!(
            "td-bar-tick-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let framebuffer =
            crate::framebuffer::Framebuffer::test_file(&path, 320, 200, 320 * 4).unwrap();
        let runtime = Mutex::new(crate::runtime::Runtime::new(framebuffer));
        let fixture = Fixture::new("0.42 0.31 0.28 2/517 9182\n", MEMINFO, "187245.31 91.2\n");
        let mut reported = Reported::default();
        // Each tick gets its own second, or the line would not change and no
        // paint would be attempted at all.
        let mut at = 1_770_000_000u64;
        let mut next = |reported: &mut Reported| {
            at = at.saturating_add(1);
            tick(&runtime, &fixture.proc_root, &fixture.sys_root, Some(at), reported)
        };

        assert_eq!(next(&mut reported), Tick::Continue(None), "a good paint says nothing");

        runtime.lock().unwrap().fail_next_repaint();
        let Tick::Continue(Some(first)) = next(&mut reported) else {
            panic!("the first failure was not reported");
        };
        assert!(first.contains("injected framebuffer paint failure"));

        // Same fault again: reported once, not once a second.
        runtime.lock().unwrap().fail_next_repaint();
        assert_eq!(next(&mut reported), Tick::Continue(None));

        // It paints, and then fails the same way again — which is news.
        assert_eq!(next(&mut reported), Tick::Continue(None));
        runtime.lock().unwrap().fail_next_repaint();
        assert!(matches!(next(&mut reported), Tick::Continue(Some(_))));

        let _ = std::fs::remove_file(&path);
    }

    /// The inverse, for the round-trip above only.
    fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
        let year = if month <= 2 { year - 1 } else { year };
        let era = year / 400;
        let year_of_era = year - era * 400;
        let month_prime = if month > 2 { month - 3 } else { month + 9 };
        let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
        let day_of_era =
            year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    #[test]
    fn every_character_the_line_can_spell_is_in_the_font() {
        // The decimal point is the one that mattered: an unmapped byte drew a
        // glyph shaped like `?`, which is exactly what this line prints for a
        // reading it could not take — so `LOAD 0.42` and `LOAD ?` looked the
        // same. Both a full line and an all-failed one are checked, since the
        // failure marker is a character of the vocabulary too.
        let full = line(&Readings {
            load_centi: Some(1_234),
            used_kb: Some(2_800_000),
            total_kb: Some(8_039_384),
            uptime_secs: Some(187_245),
            epoch_secs: Some(1_770_000_000),
            link: Link {
                name: Some("eth0".to_string()),
                up: Some(true),
                address: Address::Known("10.0.2.15".to_string()),
            },
        });
        let failed = line(&Readings::default());
        // `DOWN` spells a `W` that neither of the others does, and the
        // interface NAME is a kernel-supplied string flowing into a 43-glyph
        // font — `dev_valid_name` forbids only `/`, `:` and whitespace, so a
        // name is not guaranteed to be spellable and this is what would say
        // so.
        let down = line(&Readings {
            link: Link {
                name: Some("br-1a2b3c".to_string()),
                up: Some(false),
                address: Address::Absent,
            },
            ..Readings::default()
        });
        for text in [full.as_str(), failed.as_str(), down.as_str()] {
            for byte in text.bytes() {
                assert!(ui::is_mapped(byte), "{:?} in {text:?} has no glyph", byte as char);
            }
        }
        assert!(full.contains('.') && failed.contains('?') && down.contains("DOWN"));
    }

    #[test]
    fn the_strip_paints_only_its_own_rows() {
        let (width, height) = (400usize, 200usize);
        let stride = width * 4;
        let mut frame = vec![0u8; stride * height];
        paint(&mut frame, width, height, stride, &[1, 2], 1, "LOAD 0.42");
        for y in 0..height {
            for x in 0..width {
                let offset = y * stride + x * 4;
                let Some(pixel) = frame.get(offset..offset + 4) else {
                    continue;
                };
                if pixel.iter().any(|byte| *byte != 0) {
                    assert!(y < BAR_HEIGHT, "painted row {y} below the bar");
                }
            }
        }
        // And it fills its rows rather than leaving the desktop showing
        // between glyphs.
        assert!(frame
            .get(..stride)
            .is_some_and(|row| row.iter().any(|byte| *byte != 0)));

        // The text sits WHOLLY inside the band, which "only its own rows"
        // does not say: `draw_text_clipped`'s clip rect IS the band, so a
        // `TEXT_TOP` that cut every glyph in half would clip rather than
        // overflow and nothing above would notice.
        //
        // Scanned from where the STATUS starts, since the active workspace's
        // cell is a block of the same ink filling the band's full height.
        let text_left = desk_width(1) + desk_width(2) + TEXT_LEFT;
        let rows: Vec<usize> = (0..height)
            .filter(|y| {
                (text_left..width).any(|x| {
                    let offset = y * stride + x * 4;
                    frame.get(offset..offset + 4) == Some(&INK[..])
                })
            })
            .collect();
        assert_eq!(rows.first(), Some(&TEXT_TOP), "the text does not start at TEXT_TOP");
        assert_eq!(
            rows.last(),
            Some(&(TEXT_TOP + ui::GLYPH_HEIGHT * SCALE - 1)),
            "the text is clipped inside its own band"
        );
    }

    #[test]
    fn an_output_shorter_than_the_bar_still_clips() {
        // The band is 24 rows and the output is 8, so every clip in the path
        // has to hold. The buffer carries rows the output does not, which is
        // what makes "clips" observable: a length assertion could not fail.
        let (width, height, guard) = (64usize, 8usize, 4usize);
        let stride = width * 4;
        let mut frame = vec![0u8; stride * (height + guard)];
        paint(
            &mut frame,
            width,
            height,
            stride,
            &[1],
            1,
            "LOAD 0.42  MEM 2.7G/7.7G",
        );
        assert!(frame
            .get(stride * height..)
            .is_some_and(|rows| rows.iter().all(|byte| *byte == 0)));
        // Clipped, not SKIPPED: an output too short for the band still gets
        // every row of it, or a small screen would show the desktop where the
        // bar's rows are while the tiling area is reserved for it anyway.
        // Read in the gap between the workspace cell and the status line,
        // which is the band's own colour whatever either of them draws.
        let gap = desk_width(1) + TEXT_LEFT / 2;
        for y in 0..height {
            assert_eq!(
                frame.get(y * stride + gap * 4..y * stride + gap * 4 + 4),
                Some(&BACKGROUND[..]),
                "row {y} is not the bar"
            );
        }
    }
}
