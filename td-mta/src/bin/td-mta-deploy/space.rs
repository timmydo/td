//! The free space a job needs before it writes a base backup, a trial
//! copy, a physical recovery copy or a new closure, and its refusal when
//! a filesystem lacks it. The computation is pure; what it reads, a
//! command runner supplies: the controller's own, or the launcher's over
//! SSH and sudo.
use crate::common::{fmt, owned, set};
use crate::physical::{GIB, PGDATA, VOLUME};
use crate::R;
use td_json::Json;

/// What every filesystem a job writes keeps free once its writes are done:
/// room for the server's checkpoints, the services' own growth and logs.
pub(crate) const RESERVE: u64 = 4 * GIB;
/// What the WAL receiver keeps free on the recovery volume while it runs.
pub(crate) const RECEIVER_FREE: u64 = 8 * GIB;
/// A recovery copy's WAL: the receiver's spool budget, its replay archive
/// and the replay trial's restored copy of it.
pub(crate) const WAL: u64 = 3 * GIB;
/// What a return to the Debian server, which writes nothing, keeps free
/// for the restarted server's checkpoint and the services' resumed writes.
pub(crate) const RESTART: u64 = GIB;
/// A recovery copy's bootstrap archive: the live release, its PostgreSQL
/// and its configuration.
pub(crate) const BOOTSTRAP: u64 = GIB;
/// How every refusal begins.
pub(crate) const REFUSAL: &str = "too little free space";
const STORE: &str = "/td/store";
const JOBS: &str = "/var/lib/td-mta-operator";

/// Where a job writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// The private recovery volume: base backups, trials, WAL, archives.
    Recovery,
    /// The live data directory's filesystem: the primary's cold copy, a GIN
    /// rebuild and the server's own writes.
    Data,
    /// The unpacked closure.
    Store,
    /// The job directories, where the launcher uploads NARs.
    Jobs,
}

impl Place {
    pub(crate) fn path(self) -> &'static str {
        match self {
            Place::Recovery => VOLUME,
            Place::Data => PGDATA,
            Place::Store => STORE,
            Place::Jobs => JOBS,
        }
    }
}

/// `copies` × `size` bytes of `what` at `place`, while the job keeps
/// `keep` bytes free there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) place: Place,
    pub(crate) copies: u64,
    pub(crate) size: u64,
    pub(crate) what: &'static str,
    pub(crate) keep: u64,
}

/// The sizes a job's writes derive from, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Sizes {
    /// The live data directory's allocated size, its WAL included.
    pub(crate) data: u64,
    /// Its `pg_wal`'s, which a base backup streams afresh instead.
    pub(crate) wal: u64,
    /// The GIN indexes a minor-release note rebuilds; 0 for none.
    pub(crate) gin: u64,
    /// The NARs the launcher uploads into the job directory.
    pub(crate) upload: u64,
    /// The NARs of closure paths not yet in the store, the measure of what
    /// the job unpacks there; block rounding comes out of the reserve.
    pub(crate) unpack: u64,
}

/// A job that writes copies or a closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Job {
    /// A migrating activation: the physical recovery copy, the migration
    /// and replay trials beside it, and the primary's cold copy.
    Migration,
    /// The switch to a td-built PostgreSQL: the rehearsal's base backup and
    /// trial, the closure and any GIN rebuild.
    Switch,
    /// The return to the Debian server, which copies nothing.
    ToDebian,
    /// The launcher's NAR upload ahead of a switch.
    Upload,
}

impl Job {
    /// Whether its writes derive from the data directory's size.
    pub(crate) fn measures_data(self) -> bool {
        matches!(self, Job::Migration | Job::Switch)
    }
    fn name(self) -> &'static str {
        match self {
            Job::Migration => "the migration's recovery copy and trials",
            Job::Switch => "the PostgreSQL switch's rehearsal and closure",
            Job::ToDebian => "the return to the Debian PostgreSQL",
            Job::Upload => "the PostgreSQL closure's upload",
        }
    }
}

const fn write(place: Place, copies: u64, size: u64, what: &'static str, keep: u64) -> Write {
    Write {
        place,
        copies,
        size,
        what,
        keep,
    }
}

/// What `job` writes where, from `sizes`.
pub(crate) fn writes(job: Job, sizes: &Sizes) -> R<Vec<Write>> {
    // A base backup leaves the live pg_wal behind and streams its own.
    let base = sizes.data.saturating_sub(sizes.wal);
    let list = [
        (
            Job::Migration,
            write(
                Place::Recovery,
                3,
                base,
                "base backup, migration trial and replay trial",
                RECEIVER_FREE,
            ),
        ),
        (
            // Rewrites and the trial's own WAL, up to another copy.
            Job::Migration,
            write(Place::Recovery, 1, base, "migration trial growth", 0),
        ),
        (
            Job::Migration,
            write(Place::Recovery, 1, WAL, "WAL spool, archive and replay", 0),
        ),
        (
            Job::Migration,
            write(Place::Recovery, 1, BOOTSTRAP, "bootstrap archive", 0),
        ),
        (
            Job::Migration,
            write(
                Place::Data,
                1,
                sizes.data,
                "cold copy of the data directory",
                RESERVE,
            ),
        ),
        (
            Job::Switch,
            write(
                Place::Recovery,
                2,
                base,
                "rehearsal base backup and trial",
                RESERVE,
            ),
        ),
        (
            Job::Switch,
            write(
                Place::Recovery,
                2,
                sizes.gin,
                "trial GIN rebuild and its WAL",
                0,
            ),
        ),
        (
            Job::Switch,
            write(Place::Store, 1, sizes.unpack, "unpacked closure", RESERVE),
        ),
        (
            Job::Switch,
            write(
                Place::Data,
                2,
                sizes.gin,
                "rebuilt GIN indexes and their WAL",
                RESERVE,
            ),
        ),
        (
            Job::ToDebian,
            write(Place::Data, 0, 0, "PostgreSQL restart", RESTART),
        ),
        (
            Job::Upload,
            write(Place::Jobs, 1, sizes.upload, "uploaded NARs", RESERVE),
        ),
        (
            Job::Upload,
            write(Place::Store, 1, sizes.unpack, "unpacked closure", RESERVE),
        ),
    ];
    let mut out = Vec::new();
    for (owner, item) in list {
        if owner == job {
            td_alloc::push(&mut out, item)?;
        }
    }
    Ok(out)
}

/// A filesystem: its device, where it is mounted and what it has free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Volume {
    pub(crate) device: u64,
    pub(crate) mount: String,
    pub(crate) available: u64,
}

/// What a job needs on one filesystem against what it has.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) mount: String,
    pub(crate) needed: u64,
    pub(crate) available: u64,
    /// The writes and the space kept that make up `needed`.
    pub(crate) parts: String,
}

/// `bytes` in GiB to a tenth, rounded up or down.
fn gib(bytes: u64, up: bool) -> R<String> {
    let scaled = u128::from(bytes) * 10;
    let tenths = if up {
        scaled.div_ceil(u128::from(GIB))
    } else {
        scaled / u128::from(GIB)
    };
    fmt(format_args!("{}.{} GiB", tenths / 10, tenths % 10))
}

/// The needs of `writes`, filesystem by filesystem in the order first
/// written: each write's bytes, plus the most any write keeps free there.
/// `volume` names the filesystem holding a place.
pub(crate) fn assess(
    writes: &[Write],
    mut volume: impl FnMut(Place) -> R<Volume>,
) -> R<Vec<Verdict>> {
    struct Total<'a> {
        volume: Volume,
        bytes: u64,
        keep: u64,
        writes: Vec<&'a Write>,
    }
    let mut seen: Vec<(Place, u64)> = Vec::new();
    let mut totals: Vec<Total<'_>> = Vec::new();
    for item in writes {
        let device = match seen.iter().find(|(place, _)| *place == item.place) {
            Some((_, device)) => *device,
            None => {
                let found = volume(item.place)?;
                let device = found.device;
                td_alloc::push(&mut seen, (item.place, device))?;
                if !totals.iter().any(|t| t.volume.device == device) {
                    td_alloc::push(
                        &mut totals,
                        Total {
                            volume: found,
                            bytes: 0,
                            keep: 0,
                            writes: Vec::new(),
                        },
                    )?;
                }
                device
            }
        };
        let total = totals
            .iter_mut()
            .find(|t| t.volume.device == device)
            .ok_or("free space total")?;
        total.bytes = item
            .copies
            .checked_mul(item.size)
            .and_then(|n| n.checked_add(total.bytes))
            .ok_or("free space size overflow")?;
        total.keep = total.keep.max(item.keep);
        td_alloc::push(&mut total.writes, item)?;
    }
    let mut verdicts = Vec::new();
    for total in totals {
        let mut parts = String::new();
        for item in &total.writes {
            if item.copies == 0 || item.size == 0 {
                continue;
            }
            if !parts.is_empty() {
                td_alloc::push_str(&mut parts, " + ")?;
            }
            if item.copies > 1 {
                td_alloc::push_str(&mut parts, &fmt(format_args!("{} × ", item.copies))?)?;
            }
            td_alloc::push_str(
                &mut parts,
                &fmt(format_args!("{} {}", gib(item.size, true)?, item.what))?,
            )?;
        }
        if !parts.is_empty() {
            td_alloc::push_str(&mut parts, " + ")?;
        }
        td_alloc::push_str(
            &mut parts,
            &fmt(format_args!("{} kept free", gib(total.keep, true)?))?,
        )?;
        td_alloc::push(
            &mut verdicts,
            Verdict {
                needed: total
                    .bytes
                    .checked_add(total.keep)
                    .ok_or("free space size overflow")?,
                mount: total.volume.mount,
                available: total.volume.available,
                parts,
            },
        )?;
    }
    Ok(verdicts)
}

/// The refusal of `job` when a filesystem of `verdicts` has no more free
/// than it needs, as the running guards require more than theirs: needed
/// against available, mount by mount. It names no command: freeing space
/// is the operator's to choose.
pub(crate) fn refusal(job: Job, verdicts: &[Verdict]) -> R<Option<String>> {
    let mut short = String::new();
    for verdict in verdicts.iter().filter(|v| v.available <= v.needed) {
        if !short.is_empty() {
            td_alloc::push_str(&mut short, "; ")?;
        }
        td_alloc::push_str(
            &mut short,
            &fmt(format_args!(
                "{} needs {} ({}) and has {} available",
                verdict.mount,
                gib(verdict.needed, true)?,
                verdict.parts,
                gib(verdict.available, false)?
            ))?,
        )?;
    }
    if short.is_empty() {
        return Ok(None);
    }
    Ok(Some(fmt(format_args!(
        "{REFUSAL} for {}: {short}. Nothing was copied: free space there, then repeat the job (td-mta/PRODUCTION.md, \"Free space for copies\")",
        job.name()
    ))?))
}

/// `verdicts` as the receipt records them, in bytes.
pub(crate) fn json(verdicts: &[Verdict]) -> R<Json> {
    let mut rows = Vec::new();
    for verdict in verdicts {
        let mut row = Json::Obj(Vec::new());
        set(&mut row, "mount", Json::Str(owned(&verdict.mount)?))?;
        set(&mut row, "needed", Json::from(verdict.needed))?;
        set(&mut row, "available", Json::from(verdict.available))?;
        set(&mut row, "parts", Json::Str(owned(&verdict.parts)?))?;
        td_alloc::push(&mut rows, row)?;
    }
    Ok(Json::Arr(rows))
}

/// A command run as root in the C locale: its exit code, if it exited,
/// and its standard output, trimmed.
pub(crate) type Run<'a> = dyn Fn(&[&str]) -> R<(Option<i32>, String)> + 'a;

/// How a command that exited `code` ended, for a message.
fn ended(code: Option<i32>) -> R<String> {
    match code {
        Some(code) => fmt(format_args!("exit {code}")),
        None => owned("killed by a signal"),
    }
}

/// The output of a command that must succeed.
fn out(run: &Run<'_>, args: &[&str]) -> R<String> {
    match run(args)? {
        (Some(0), output) => Ok(output),
        (code, _) => Err(fmt(format_args!(
            "{} failed ({}) measuring free space",
            args.first().copied().unwrap_or_default(),
            ended(code)?
        ))?
        .into()),
    }
}

/// The first field of `du -s -B1` output: allocated bytes.
pub(crate) fn du_bytes(output: &str) -> R<u64> {
    Ok(output
        .split_ascii_whitespace()
        .next()
        .ok_or("du reported no size")?
        .parse()?)
}

/// The allocated size of `path` by `du`, which also exits 1 when a file
/// of the live cluster vanishes while it walks, still printing the total.
fn du(run: &Run<'_>, path: &str) -> R<u64> {
    match run(&["du", "-s", "-x", "-B1", "--", path])? {
        (Some(0 | 1), output) => du_bytes(&output),
        (code, _) => Err(fmt(format_args!("du {path} failed ({})", ended(code)?))?.into()),
    }
}

/// The data directory's allocated size and its `pg_wal`'s; `du` counts a
/// directory once, so each is its own call.
pub(crate) fn data_sizes(run: &Run<'_>) -> R<(u64, u64)> {
    let wal = fmt(format_args!("{PGDATA}/pg_wal"))?;
    Ok((du(run, PGDATA)?, du(run, &wal)?))
}

/// The mount point and available bytes of `df -B1 --output=target,avail`
/// for one path.
pub(crate) fn df_row(output: &str) -> R<(String, u64)> {
    let mut lines = output.lines();
    let header = lines.next().ok_or("df reported nothing")?;
    let row = lines.next().ok_or("df reported no filesystem")?;
    let (target, avail) = row
        .trim()
        .rsplit_once(|c: char| c.is_ascii_whitespace())
        .ok_or("df row")?;
    let target = target.trim_end();
    if !header.trim_start().starts_with("Mounted on")
        || lines.next().is_some()
        || !target.starts_with('/')
    {
        return Err("unexpected df output".into());
    }
    Ok((owned(target)?, avail.parse()?))
}

/// The filesystem holding `place`; a path not there yet is on its nearest
/// ancestor's.
pub(crate) fn volume(run: &Run<'_>, place: Place) -> R<Volume> {
    let mut path = std::path::Path::new(place.path());
    loop {
        match run(&["test", "-e", path.to_str().ok_or("path")?])?.0 {
            Some(0) => break,
            Some(1) => path = path.parent().ok_or("no existing ancestor")?,
            code => {
                return Err(fmt(format_args!(
                    "test -e {} failed ({}) measuring free space",
                    path.display(),
                    ended(code)?
                ))?
                .into())
            }
        }
    }
    let text = path.to_str().ok_or("path")?;
    let device = out(run, &["stat", "-L", "-c", "%d", "--", text])?.parse()?;
    let (mount, available) = df_row(&out(
        run,
        &["df", "-B1", "--output=target,avail", "--", text],
    )?)?;
    Ok(Volume {
        device,
        mount,
        available,
    })
}

#[cfg(test)]
#[path = "tests/space.rs"]
mod tests;
