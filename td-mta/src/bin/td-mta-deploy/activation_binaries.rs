//! Fenced switch of the production server from the extracted Debian
//! PostgreSQL to td's source-built one, on the same data directory. The
//! launcher built the closure (the PostgreSQL output and td glibc) and sent
//! each store path as a NAR; it enters /td/store only verified. Nothing in
//! production changes before the td server passes a trial on a fresh copy.
//! The Debian tree stays for rollback.
use super::*;
use crate::{
    common::{banner_minor, free_bytes, postgres_minor, store_basename, DEBIAN_TOOLS},
    isolation::files::{self, Setting},
    physical::SOCKET_DIR,
    postgres_minors::{self, Check, Note, GIN_BYTES, GIN_INDEXES, REINDEX_GIN},
};
use database::OWNER_SERVICE;
use fence::{open_socket, LiveSocket, SocketFence};
use transport::{check_socket, read_file, role_name, CONF_LIMIT, FILE_LIMIT};

#[path = "activation_loader.rs"]
mod loader;
#[path = "activation_store.rs"]
mod store;

pub(crate) use store::discard_nars;

pub(crate) const BINARIES_DROPIN: &str =
    "/etc/systemd/system/td-mta-postgres.service.d/91-td-mta-binaries.conf";
const STORE: &str = "/td/store";
const PG_BUS: &str = "/org/freedesktop/systemd1/unit/td_2dmta_2dpostgres_2eservice";
const DROPIN_HEAD: &str = "[Service]\nExecStart=\nExecStart=:";
// td's loader must resolve the server's libraries through its RUNPATH alone.
const UNSET: &str = "UnsetEnvironment=LD_LIBRARY_PATH LD_PRELOAD LD_AUDIT\n";
const UNSET_NAMES: &[&str] = &["LD_LIBRARY_PATH", "LD_PRELOAD", "LD_AUDIT"];
/// What the controller and the server run from the selected tree.
const PROGRAMS: &[&str] = &[
    "postgres",
    "psql",
    "pg_dump",
    "pg_basebackup",
    "pg_verifybackup",
    "pg_receivewal",
    "pg_isready",
    "pg_controldata",
];
const TRIAL_ADMIN: &str = "local all setup_admin peer map=trial\n";

/// Gates 1 to 3 for the whole cluster, and the server's release.
pub(super) const CLUSTER_GATES: &str = "SELECT \
     (SELECT count(*) FROM pg_database WHERE datlocprovider = 'i'), \
     current_setting('default_toast_compression'), current_setting('wal_compression'), \
     current_setting('server_version_num')";
/// For one database: columns and indexes on ICU collations, other objects
/// that depend on one (views, defaults, constraints, partition keys),
/// columns set to lz4, unlogged tables with a variable-length column (a base
/// backup copies them empty, so the trial's scan cannot read their values),
/// used libc collations whose recorded version differs from what the running
/// server's C library reports, and installed extensions the running server
/// cannot load.
pub(super) const DATABASE_GATES: &str = "SELECT \
     (SELECT count(*) FROM pg_attribute a JOIN pg_collation c ON c.oid = a.attcollation \
      WHERE NOT a.attisdropped AND c.collprovider = 'i') \
     + (SELECT count(*) FROM pg_index i JOIN pg_collation c ON c.oid = ANY (i.indcollation::oid[]) \
      WHERE c.collprovider = 'i'), \
     (SELECT count(*) FROM pg_depend d JOIN pg_collation c ON c.oid = d.refobjid \
      WHERE d.refclassid = 'pg_collation'::regclass AND c.collprovider = 'i'), \
     (SELECT count(*) FROM pg_attribute WHERE NOT attisdropped AND attcompression = 'l'), \
     (SELECT count(*) FROM pg_class c WHERE c.relpersistence = 'u' AND c.relkind IN ('r', 'm') \
      AND EXISTS (SELECT FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 \
       AND NOT a.attisdropped AND a.attlen = -1)), \
     (SELECT count(*) FROM pg_collation c WHERE c.collprovider = 'c' \
      AND c.collversion IS DISTINCT FROM pg_collation_actual_version(c.oid) \
      AND (EXISTS (SELECT FROM pg_attribute a WHERE a.attcollation = c.oid AND NOT a.attisdropped) \
       OR EXISTS (SELECT FROM pg_index i WHERE c.oid = ANY (i.indcollation::oid[])))), \
     (SELECT count(*) FROM pg_extension e WHERE NOT EXISTS (SELECT FROM pg_available_extension_versions v \
      WHERE v.name = e.extname AND v.version = e.extversion))";
/// As the trial's superuser: that role, every database's default collation
/// version under td glibc, and the trial's release.
pub(super) const TRIAL_CLUSTER: &str = "SELECT \
     (SELECT rolsuper FROM pg_roles WHERE rolname = current_user), \
     (SELECT count(*) FROM pg_database \
      WHERE datcollversion IS DISTINCT FROM pg_database_collation_actual_version(oid)), \
     current_setting('server_version_num')";
pub(super) const DATABASES: &str =
    "SELECT datname FROM pg_database WHERE datallowconn ORDER BY datname";
/// Gate 2 in full: every stored value of every table and populated
/// materialized view, as `rel.column` per lz4-compressed column, or
/// `scanned` when there is none.
pub(super) const LZ4_SCAN: &str = "SET statement_timeout = 0;
DO $$
DECLARE r record; found bigint; offenders text[] := '{}';
BEGIN
  FOR r IN SELECT c.oid::regclass AS rel, a.attname FROM pg_class c
           JOIN pg_attribute a ON a.attrelid = c.oid
           WHERE c.relkind IN ('r', 'm') AND c.relispopulated
             AND a.attnum > 0 AND NOT a.attisdropped AND a.attlen = -1 LOOP
    EXECUTE format('SELECT count(*) FROM %s WHERE pg_column_compression(%I) = %L',
                   r.rel, r.attname, 'lz4') INTO found;
    IF found <> 0 THEN
      offenders := offenders || format('%s.%I', r.rel, r.attname);
    END IF;
  END LOOP;
  PERFORM set_config('td_mta.lz4_values', array_to_string(offenders, E'\\n'), false);
END
$$;
SELECT coalesce(nullif(current_setting('td_mta.lz4_values'), ''), 'scanned');
";
/// pg_trgm loads and serves td-mta's search index. With sequential and plain
/// index scans off, the only plan for the pattern is the trigram index.
pub(super) const TRIGRAM: &str = "SET statement_timeout = '20s';
SET enable_seqscan = off;
SET enable_indexscan = off;
SET enable_indexonlyscan = off;
SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm';
SELECT td_mta_trgm.similarity('postgresql', 'postgres') > 0.5;
EXPLAIN (COSTS OFF) SELECT count(*) FROM td_mta.blob_search
  WHERE candidate_text LIKE '%td-mta binary probe%';
SELECT count(*) FROM td_mta.blob_search WHERE candidate_text LIKE '%td-mta binary probe%';
";

pub(super) struct Entry {
    pub(super) basename: String,
    pub(super) nar_sha256: String,
}

/// The job's closure, PostgreSQL first, and the live release it replaces.
pub(super) struct Closure {
    pub(super) postgres: Entry,
    pub(super) glibc: Entry,
    /// N of the td-built 17.N.
    pub(super) minor: u32,
    /// The live Debian minor the launcher read, which the minor-release
    /// table covers up to 17.N.
    pub(super) live: u32,
}

fn entry(json: &Json) -> R<Entry> {
    let basename = field(json, "basename")?;
    let hash = field(json, "nar_sha256")?;
    require(lower_hex(hash, 64), "invalid NAR digest")?;
    Ok(Entry {
        basename: owned(basename)?,
        nar_sha256: owned(hash)?,
    })
}

impl Closure {
    pub(super) fn from_job(job: &Json) -> R<Self> {
        let Some([postgres, glibc]) = job.get("closure").and_then(Json::as_arr) else {
            return Err("the job must name the PostgreSQL output and td glibc".into());
        };
        let (postgres, glibc) = (entry(postgres)?, entry(glibc)?);
        let minor = postgres_minor(store_basename(&postgres.basename, "postgresql-x86-64")?)?;
        store_basename(&glibc.basename, "glibc-x86-64")?;
        let live = postgres_minor(field(job, "postgres_launcher_minor")?)?;
        postgres_minors::movable(live, minor)?;
        Ok(Self {
            postgres,
            glibc,
            minor,
            live,
        })
    }

    /// The minor-release notes from the live release up to td's.
    fn notes(&self) -> R<impl Iterator<Item = (u32, &'static Note)>> {
        postgres_minors::span(self.live, self.minor)
    }

    fn rebuilds_gin(&self) -> R<bool> {
        Ok(self
            .notes()?
            .any(|(_, note)| matches!(note.check, Check::ReindexGin)))
    }

    fn basenames(&self) -> [&str; 2] {
        [&self.postgres.basename, &self.glibc.basename]
    }

    fn bin(&self) -> R<PathBuf> {
        child(
            Path::new(STORE),
            &fmt(format_args!("{}/bin", self.postgres.basename))?,
        )
    }

    fn postgres(&self) -> R<PathBuf> {
        child(&self.bin()?, "postgres")
    }
}

/// N of a 17.N server's `server_version_num`.
pub(super) fn number_minor(number: &str) -> R<u32> {
    let number = number.parse::<u32>()?;
    require(
        (170_000..180_000).contains(&number),
        "not a PostgreSQL 17 server",
    )?;
    Ok(number - 170_000)
}

// An argument systemd passes through unchanged: no specifier, variable,
// quote, escape or separator.
fn plain(arg: &str) -> bool {
    !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_.=,:+@-".contains(&b))
}

/// The arguments after argv[0] of the one command systemd runs for
/// PostgreSQL, which must be `program` itself.
pub(super) fn server_arguments<'a>(property: &'a Json, program: &str) -> R<Vec<&'a str>> {
    let Some([row]) = property.get("data").and_then(Json::as_arr) else {
        return Err("PostgreSQL must run exactly one command".into());
    };
    let Some([path, argv, flags, ..]) = row.as_arr() else {
        return Err("unexpected ExecStartEx row".into());
    };
    let flags = flags.as_arr().ok_or("unexpected ExecStartEx flags")?;
    require(
        flags.len() <= 1
            && flags
                .iter()
                .all(|flag| flag.as_str() == Some("no-env-expand")),
        "unexpected PostgreSQL command flags",
    )?;
    let Some((first, rest)) = argv.as_arr().and_then(|argv| argv.split_first()) else {
        return Err("unexpected ExecStartEx arguments".into());
    };
    require(
        path.as_str() == Some(program) && first.as_str() == Some(program),
        "td-mta-postgres.service runs another program",
    )?;
    let mut args = Vec::new();
    for arg in rest {
        let arg = arg.as_str().ok_or("unexpected ExecStartEx argument")?;
        require(plain(arg), "unexpected PostgreSQL argument")?;
        td_alloc::push(&mut args, arg)?;
    }
    Ok(args)
}

/// The drop-in that runs `program` with `args` in place of the unit's own
/// command, without the loader variables.
pub(super) fn dropin_text(program: &str, args: &[&str]) -> R<String> {
    let mut text = owned(DROPIN_HEAD)?;
    td_alloc::push_str(&mut text, program)?;
    for arg in args {
        require(plain(arg), "unexpected PostgreSQL argument")?;
        td_alloc::push_str(&mut text, " ")?;
        td_alloc::push_str(&mut text, arg)?;
    }
    td_alloc::push_str(&mut text, "\n")?;
    td_alloc::push_str(&mut text, UNSET)?;
    Ok(text)
}

/// The td-built server a drop-in `dropin_text` wrote runs.
pub(super) fn dropin_program(text: &str) -> R<&str> {
    let (command, tail) = text
        .strip_prefix(DROPIN_HEAD)
        .and_then(|rest| rest.split_once('\n'))
        .ok_or("unexpected PostgreSQL binaries drop-in")?;
    let mut words = command.split(' ');
    let program = words
        .next()
        .ok_or("unexpected PostgreSQL binaries drop-in")?;
    require(
        tail == UNSET && words.all(plain),
        "unexpected PostgreSQL binaries drop-in",
    )?;
    let basename = program
        .strip_prefix("/td/store/")
        .and_then(|rest| rest.strip_suffix("/bin/postgres"))
        .ok_or("the PostgreSQL binaries drop-in names no td-built server")?;
    store_basename(basename, "postgresql-x86-64")?;
    Ok(program)
}

/// The td-built tools' directory if the drop-in selects them.
pub(super) fn selected() -> R<Option<PathBuf>> {
    let path = Path::new(BINARIES_DROPIN);
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
        Ok(_) => (),
    }
    files::safe_parents(path)?;
    let (bytes, meta) = read_file(path, FILE_LIMIT)?;
    require(
        meta.uid() == 0 && meta.gid() == 0 && meta.mode() & 0o7777 == 0o644,
        "PostgreSQL binaries drop-in ownership or mode",
    )?;
    let program = Path::new(dropin_program(std::str::from_utf8(&bytes)?)?);
    files::trusted_binary(program)?;
    let bin = parent(program)?.to_str().ok_or("store path")?;
    Ok(Some(PathBuf::from(owned(bin)?)))
}

fn columns<const N: usize>(row: &str) -> R<[&str; N]> {
    let mut out = [""; N];
    let mut parts = row.split('|');
    for slot in &mut out {
        *slot = parts.next().ok_or("short gate result")?;
    }
    require(parts.next().is_none(), "long gate result")?;
    Ok(out)
}

/// Gates 1 to 3 from `CLUSTER_GATES`; the live release's minor.
pub(super) fn cluster_gates(row: &str) -> R<u32> {
    let [icu, toast, wal, version] = columns(row)?;
    require(
        icu == "0",
        "gate 1: a database uses the ICU locale provider",
    )?;
    require(
        toast == "pglz",
        "gate 2: default_toast_compression is not pglz",
    )?;
    require(
        matches!(wal, "off" | "on" | "pglz"),
        "gate 3: wal_compression names a method td's server lacks",
    )?;
    number_minor(version)
}

/// One database's `DATABASE_GATES`. The collation versions count only on
/// the td-built trial: the Debian server compares against its own library.
pub(super) fn database_gates(row: &str, trial: bool) -> R<()> {
    let [icu, depends, lz4, unlogged, versions, extensions] = columns(row)?;
    require(
        icu == "0",
        "gate 1: a column or index uses an ICU collation",
    )?;
    require(
        depends == "0",
        "gate 1: an object depends on an ICU collation",
    )?;
    require(lz4 == "0", "gate 2: a column is set to lz4 compression")?;
    require(
        unlogged == "0",
        "gate 2: an unlogged table's values are outside the trial's lz4 scan",
    )?;
    require(
        extensions == "0",
        "an installed extension is unavailable to the td-built server",
    )?;
    require(
        !trial || versions == "0",
        "a used collation's version differs under td glibc",
    )
}

/// The lz4-compressed columns `LZ4_SCAN` names; none if it printed
/// `scanned`.
pub(super) fn lz4_values(output: &str) -> R<Vec<&str>> {
    let mut found = Vec::new();
    if output != "scanned" {
        for line in output.lines() {
            require(line.contains('.'), "gate 2: unexpected lz4 scan output")?;
            td_alloc::push(&mut found, line)?;
        }
        require(!found.is_empty(), "gate 2: unexpected lz4 scan output")?;
    }
    Ok(found)
}

/// `TRIAL_CLUSTER`; the trial's minor.
pub(super) fn trial_cluster(row: &str) -> R<u32> {
    let [superuser, versions, version] = columns(row)?;
    require(
        superuser == "t",
        "the trial's setup_admin is not a superuser",
    )?;
    require(
        versions == "0",
        "a database's collation version differs under td glibc",
    )?;
    number_minor(version)
}

/// `TRIGRAM`'s output: the extension, its function, a plan on the trigram
/// index and a count.
pub(super) fn trigram(output: &str) -> R<()> {
    let mut lines = output.lines();
    require(
        lines.next().is_some_and(|v| !v.is_empty()) && lines.next() == Some("t"),
        "pg_trgm is not usable",
    )?;
    let mut plan = false;
    let mut last = None;
    for line in lines {
        if let Some(previous) = last {
            plan |= line_uses_index(previous);
        }
        last = Some(line);
    }
    require(
        plan && last.is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        "the trigram index does not serve td-mta's search",
    )
}

const LIVE_CHANGED: &str = "the live Debian PostgreSQL is not the release the launcher read: run td-mta/deploy --postgres-binaries again";

/// A minor-release note's count, which must be zero.
pub(super) fn concerned(release: u32, note: &Note, count: &str) -> R<()> {
    if count == "0" {
        return Ok(());
    }
    count
        .parse::<u64>()
        .map_err(|_| "unexpected minor-release check result")?;
    Err(fmt(format_args!(
        "PostgreSQL 17.{release} note {}: the cluster holds what it concerns, which the switch does not repair; stay on the Debian server",
        note.id
    ))?
    .into())
}

/// The count a script prints last.
pub(super) fn last_count(output: &str) -> R<u64> {
    Ok(output
        .lines()
        .last()
        .ok_or("missing count")?
        .trim()
        .parse()?)
}

/// What the receipt records of each minor-release note: `verified`,
/// `exempt` with its reason, or the GIN rebuild.
fn note_record(release: u32, note: &Note) -> R<Json> {
    let mut row = Json::Obj(Vec::new());
    set(
        &mut row,
        "release",
        Json::Str(fmt(format_args!("17.{release}"))?),
    )?;
    set(&mut row, "id", Json::Str(owned(note.id)?))?;
    let result = match note.check {
        Check::Database(_) | Check::Cluster(_) => owned("verified")?,
        // Production's rebuild is the event "rebuilt GIN indexes".
        Check::ReindexGin => owned("GIN rebuild rehearsed")?,
        Check::Exempt(reason) => fmt(format_args!("exempt: {reason}"))?,
    };
    set(&mut row, "result", Json::Str(result))?;
    Ok(row)
}

/// The rebuild's own limit, on the copy and in production.
const GIN_LIMIT: u64 = 600;

/// The window a switch needs for a GIN rebuild that took `trial` seconds
/// on the copy, twice that and half a minute for the rest, and the window
/// a refusal names: a quarter more, in whole minutes, so a rerun's
/// slightly slower trial still fits.
pub(super) fn gin_window(trial: f64) -> (u64, u64) {
    // Bounded by the rebuild's limit.
    let needed = (trial.clamp(0.0, GIN_LIMIT as f64) * 2.0 + 30.0).ceil() as u64;
    (needed, (needed + needed / 4).div_ceil(60) * 60)
}

fn line_uses_index(line: &str) -> bool {
    line.trim_start()
        .trim_start_matches("->")
        .trim_start()
        .starts_with("Bitmap Index Scan on blob_search_candidates")
}

/// Whether `path` names a file inside one of `basenames`'s store paths.
pub(super) fn in_store(path: &str, basenames: &[&str]) -> bool {
    path.strip_prefix("/td/store/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(name, rest)| {
            basenames.contains(&name)
                && rest.split('/').all(|part| !matches!(part, "" | "." | ".."))
        })
}

/// The first executable mapping in `/proc/PID/maps` that is not inside one
/// of `basenames`'s store paths.
pub(super) fn foreign_code<'a>(maps: &'a str, basenames: &[&str]) -> Option<&'a str> {
    for line in maps.lines() {
        let mut fields = line.splitn(6, ' ');
        let perms = fields.nth(1).unwrap_or("");
        let path = fields.nth(3).unwrap_or("").trim_start();
        if !perms.contains('x') || !path.starts_with('/') {
            continue;
        }
        if !in_store(path, basenames) {
            return Some(path);
        }
    }
    None
}

/// The store paths of the selected td-built tools, the PostgreSQL output
/// and the td glibc its server loads, for the bootstrap archive.
pub(crate) fn selected_store(host: &Host) -> R<Vec<PathBuf>> {
    let postgres = host.tool("postgres")?;
    let output = parent(parent(&postgres)?)?;
    let server = loader::store_root(output.to_str().ok_or("store path")?, "postgresql-x86-64")?;
    require(
        server == output,
        "the selected server is not a store path's",
    )?;
    let interpreter =
        loader::interpreter(&postgres)?.ok_or("the td-built server names no loader")?;
    let glibc = loader::store_root(&interpreter, "glibc-x86-64")?;
    let mut paths = td_alloc::with_capacity(2)?;
    td_alloc::push(&mut paths, server)?;
    td_alloc::push(&mut paths, glibc)?;
    Ok(paths)
}

/// What `unit_state` reads of td-mta-postgres.service.
const UNIT_PROPERTIES: &[&str] = &[
    "Type",
    "ExecCondition",
    "ExecStartPre",
    "ExecStartPost",
    "ExecReload",
    "ExecStop",
    "ExecStopPost",
    "Environment",
];

/// `systemctl show` of `UNIT_PROPERTIES`: a service type td's server, built
/// without systemd notification, supports, and no other command or
/// environment setting that names the Debian tree.
pub(super) fn unit_state(state: &str) -> R<()> {
    let mut kind = None;
    for line in state.lines() {
        let (name, value) = line
            .split_once('=')
            .ok_or("unexpected systemctl show output")?;
        if name == "Type" {
            kind = Some(value);
        } else if name == "Environment" {
            // The drop-in unsets the loader variables and the running server
            // is checked without them; any other variable must not name the
            // Debian tree.
            for assignment in value.split_ascii_whitespace() {
                let assignment = assignment.trim_matches('"');
                let variable = assignment.split_once('=').map_or(assignment, |(v, _)| v);
                require(
                    UNSET_NAMES.contains(&variable)
                        || !assignment.contains(crate::common::DEBIAN_POSTGRES),
                    "a td-mta-postgres.service environment variable names the Debian tree",
                )?;
            }
        } else {
            require(
                !value.contains(crate::common::DEBIAN_POSTGRES),
                "another td-mta-postgres.service setting names the Debian tree",
            )?;
        }
    }
    require(
        matches!(kind, Some("exec" | "simple")),
        "td-mta-postgres.service must be Type=exec or Type=simple",
    )
}

#[derive(Default)]
pub(super) struct Switch {
    stopped: bool,
    dropin: Option<Setting>,
}

impl Deployment {
    pub(super) fn postgres_binaries(&mut self) -> R<()> {
        require(
            !Path::new(FENCE).try_exists()?,
            "unfinished deployment fence: inspect previous receipt",
        )?;
        require(
            self.previous == self.candidate && fs::canonicalize(ACTIVE)? == self.previous,
            "the PostgreSQL binary switch keeps the live release",
        )?;
        let closure = Closure::from_job(&self.record)?;
        self.check_source()?;
        self.socket_preflight()?;
        crate::readiness::prepare(&self.host, false)?;
        if !self.host.debian_tools() {
            require(
                self.host.tool("postgres")? == closure.postgres()?,
                "PostgreSQL already runs other td-built binaries",
            )?;
            loader::check_loading(&self.host, &closure)?;
            self.td_process(self.pid(PG)?, &closure)?;
            self.journal_identity()?;
            self.config(&self.previous, &[], self.isolated)?;
            self.healthy(&self.previous)?;
            self.probes.probe("smtp")?;
            self.probes.probe("jmap")?;
            self.event("already on the td-built PostgreSQL")?;
            return Ok(());
        }
        self.debian_server(&closure)?;
        self.journal_identity()?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        let mut switch = Switch::default();
        self.guarded(
            &mut switch,
            |d, switch| d.binaries_cutover(&closure, switch),
            Self::restore_debian,
        )
    }

    // Gate 6: the live server is the release the launcher read.
    fn debian_server(&mut self, closure: &Closure) -> R<()> {
        let debian = child(Path::new(DEBIAN_TOOLS), "postgres")?;
        let exe = fs::canonicalize(fmt(format_args!("/proc/{}/exe", self.pid(PG)?))?)?;
        require(
            exe == fs::canonicalize(&debian)?,
            "the live server is not the retained Debian PostgreSQL",
        )?;
        let minor = banner_minor(&self.host.out(self.host.cmd(&debian).arg("--version"))?)?;
        set(
            &mut self.record,
            "postgres_debian_minor",
            Json::from(u64::from(minor)),
        )?;
        require(minor == closure.live, LIVE_CHANGED)
    }

    fn binaries_cutover(&mut self, closure: &Closure, switch: &mut Switch) -> R<()> {
        let setting = self.binaries_dropin(closure)?;
        self.event("installing the td-built PostgreSQL closure")?;
        let placed = store::install(&self.host, &self.work, closure)?;
        set(&mut self.record, "postgres_store", placed)?;
        let bin = closure.bin()?;
        for program in PROGRAMS {
            files::trusted_binary(&child(&bin, program)?)?;
        }
        loader::check_loading(&self.host, closure)?;
        // From here the controller's own PostgreSQL clients are td's.
        self.host.select_tools(Some(bin));
        let banner = self
            .host
            .out(self.host.cmd(closure.postgres()?).arg("--version"))?;
        require(
            banner_minor(&banner)? == closure.minor,
            "the td-built server reports another release",
        )?;
        self.event("checking PostgreSQL data gates")?;
        self.live_gates(closure)?;
        self.event("rehearsing the td-built PostgreSQL")?;
        self.rehearse_binaries(closure)?;
        self.event("td-built PostgreSQL rehearsal verified")?;

        require(
            !setting.path.try_exists()?,
            "unexpected PostgreSQL binaries drop-in",
        )?;
        let uid = fs::metadata(PGDATA)?.uid();
        let gid = isolation::db_group(&self.host, false)?;
        self.prepare_fence(true, false)?;
        self.fence(false, false)?;
        // The included file fences a restart from here on.
        self.socket_fence = true;
        LiveSocket(&self.host).write_file()?;
        switch.stopped = true;
        self.host
            .run(self.host.cmd("systemctl").args(["stop", PG]))?;
        // Gate 4: td's pg_controldata reads the Debian server's clean shutdown.
        require(
            self.control_state(Path::new(PGDATA))? == "shut down",
            "clean PostgreSQL shutdown",
        )?;
        let setting = &*switch.dropin.get_or_insert(setting);
        files::install(&self.host, std::slice::from_ref(setting))?;
        self.host
            .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
        self.check_unit(closure, &setting.text)?;
        self.host
            .run(self.host.cmd("systemctl").args(["start", PG]))?;
        self.wait(25, "td-built PostgreSQL startup", || self.ready_db(&[]))?;
        check_socket(Path::new(SOCKET_DIR), uid, gid, 0)?;
        self.check_source()?;
        self.td_process(self.pid(PG)?, closure)?;
        require(
            number_minor(&self.sql(
                "owner",
                "SELECT current_setting('server_version_num')",
                &[],
                false,
            )?)? == closure.minor,
            "the switched server reports another release",
        )?;
        if closure.rebuilds_gin()? {
            let count = self.reindex_gin(&[])?;
            require(
                self.record
                    .get("postgres_gin_indexes")
                    .and_then(Json::as_u64)
                    == Some(count),
                "production's GIN indexes differ from the trial's",
            )?;
            self.event("rebuilt GIN indexes")?;
        }
        trigram(&self.sql("owner", TRIGRAM, &[], true)?)?;
        self.event("td-built PostgreSQL started")?;
        self.admit()?;
        self.wait(240, "ACME lease recovery", || self.acme_online())?;
        self.config(&self.previous, &[], self.isolated)?;
        self.healthy(&self.previous)?;
        require(self.active_timers()? == self.timers, "timer set changed")?;
        self.event("verified td-built PostgreSQL")?;
        Ok(())
    }

    // Gates 1 to 3 on the live server, through the owner (td_mta) and the
    // marker (postgres) logins.
    fn live_gates(&self, closure: &Closure) -> R<()> {
        let minor = cluster_gates(&self.sql("owner", CLUSTER_GATES, &[], false)?)?;
        require(minor == closure.live, LIVE_CHANGED)?;
        for role in ["owner", "marker"] {
            database_gates(&self.sql(role, DATABASE_GATES, &[], false)?, false)?;
        }
        // The minor-release notes about the cluster as a whole.
        for (release, note) in closure.notes()? {
            if let Check::Cluster(count) = note.check {
                concerned(release, note, &self.sql("owner", count, &[], false)?)?;
            }
        }
        // REINDEX writes each new index beside the old one, and its WAL.
        if closure.rebuilds_gin()? {
            let size: u64 = self.sql("owner", GIN_BYTES, &[], false)?.parse()?;
            require(
                free_bytes(Path::new(PGDATA))? > size.saturating_mul(2).saturating_add(GIB),
                "too little space beside the data directory for the GIN rebuild",
            )?;
        }
        Ok(())
    }

    // As the owner: on the trial copy, then on production before writer
    // admission.
    fn reindex_gin(&self, ns: &[String]) -> R<u64> {
        let mut cmd = self.in_namespace(ns, self.host.tool("psql")?);
        cmd.args(["-XwqAt", "-v", "ON_ERROR_STOP=1", "service=td-mta-backup"])
            .env("PGSERVICEFILE", OWNER_SERVICE);
        let out = self
            .host
            .exec(
                &mut cmd,
                REINDEX_GIN.as_bytes(),
                Duration::from_secs(GIN_LIMIT),
                true,
            )
            .map_err(|e| {
                fmt(format_args!(
                    "the GIN rebuild failed (its limit: {GIN_LIMIT} seconds or the writer window): {e}"
                ))
                .map_or_else(|_| e, Into::into)
            })?;
        last_count(std::str::from_utf8(&out.stdout)?)
    }

    /// Gate 5: the td server's first start is on a fresh copy, with the
    /// production configuration and every login of the live release. Its
    /// superuser repeats the data gates on every database and scans every
    /// stored value for lz4.
    fn rehearse_binaries(&mut self, closure: &Closure) -> R<()> {
        let (conf, _) = read_file(&child(Path::new(MAIL), "postgresql.conf")?, CONF_LIMIT)?;
        let (hba, _) = read_file(&self.hba_file()?, CONF_LIMIT)?;
        let mut trial_hba = owned(TRIAL_ADMIN)?;
        td_alloc::push_str(&mut trial_hba, std::str::from_utf8(&hba)?)?;
        let postgres = closure.postgres()?;
        self.rehearsal(|d, copy| {
            require(
                d.control_field(&child(&copy.work, "base")?, "Database system identifier")?
                    == SYSTEM,
                "td's pg_controldata on the copy",
            )?;
            copy.start_staged(
                d,
                "binaries",
                &postgres,
                &conf,
                trial_hba.as_bytes(),
                b"trial root setup_admin\n",
            )?;
            let ns = copy.hold(d, "binaries", &[])?;
            d.qualify_trial(&ns)?;
            d.td_process(copy.trial_pid(&d.host)?, closure)?;
            let short = Duration::from_secs(40);
            let host = &d.host;
            require(
                trial_cluster(&copy.superuser(host, "td_mta", TRIAL_CLUSTER, short)?)?
                    == closure.minor,
                "the trial reports another release",
            )?;
            let gin = closure.rebuilds_gin()?;
            let mut rebuilt = None;
            for name in copy.superuser(host, "td_mta", DATABASES, short)?.lines() {
                require(role_name(name), "unexpected database name")?;
                database_gates(&copy.superuser(host, name, DATABASE_GATES, short)?, true)?;
                for (release, note) in closure.notes()? {
                    if let Check::Database(count) = note.check {
                        concerned(release, note, &copy.superuser(host, name, count, short)?)?;
                    }
                }
                if gin && name == "td_mta" {
                    // As production will: the owner, through its own login.
                    let start = Instant::now();
                    let count = d.reindex_gin(&ns)?;
                    rebuilt = Some((count, start.elapsed().as_secs_f64()));
                } else if gin {
                    require(
                        copy.superuser(host, name, GIN_INDEXES, short)? == "0",
                        "a GIN index outside the mail database, which the switch does not rebuild; stay on the Debian server",
                    )?;
                }
                let scan = copy.superuser(host, name, LZ4_SCAN, Duration::from_secs(3600))?;
                let found = lz4_values(&scan)?;
                if !found.is_empty() {
                    let mut values = Vec::new();
                    let mut message =
                        fmt(format_args!("gate 2: lz4-compressed values in {name}:"))?;
                    for column in &found {
                        td_alloc::push(&mut values, Json::Str(owned(column)?))?;
                        td_alloc::push_str(&mut message, " ")?;
                        td_alloc::push_str(&mut message, column)?;
                    }
                    let mut row = Json::Obj(Vec::new());
                    set(&mut row, "database", Json::Str(owned(name)?))?;
                    set(&mut row, "columns", Json::Arr(values))?;
                    set(&mut d.record, "postgres_lz4_values", row)?;
                    return Err(message.into());
                }
            }
            let mut notes = Vec::new();
            for (release, note) in closure.notes()? {
                td_alloc::push(&mut notes, note_record(release, note)?)?;
            }
            set(&mut d.record, "postgres_minor_notes", Json::Arr(notes))?;
            if let Some((count, seconds)) = rebuilt {
                set(&mut d.record, "postgres_gin_indexes", Json::from(count))?;
                set(
                    &mut d.record,
                    "postgres_gin_rebuild_trial_ms",
                    Json::from((seconds * 1000.0).round()),
                )?;
                // The production rebuild runs inside the writer window.
                let (needed, suggested) = gin_window(seconds);
                if d.budget.as_secs() < needed {
                    return Err(fmt(format_args!(
                        "the GIN rebuild needs a longer writer window: run td-mta/deploy --postgres-binaries --window-seconds {suggested}"
                    ))?
                    .into());
                }
            } else {
                require(!gin, "the mail database was not rehearsed")?;
            }
            trigram(&d.sql("owner", TRIGRAM, &ns, true)?)?;
            d.dump_schema(&ns)
        })
    }

    // td's pg_dump through the holder, as the scheduled backup's login.
    fn dump_schema(&self, ns: &[String]) -> R<()> {
        let mut cmd = self.in_namespace(ns, self.host.tool("pg_dump")?);
        cmd.args(["-w", "--schema-only", "-d", "service=td-mta-backup"])
            .env("PGSERVICEFILE", OWNER_SERVICE);
        let out = self
            .host
            .exec(&mut cmd, &[], Duration::from_secs(120), true)?;
        let complete = b"PostgreSQL database dump complete";
        require(
            out.stdout
                .windows(complete.len())
                .any(|window| window == complete),
            "td's pg_dump did not complete",
        )
    }

    /// The PostgreSQL server `pid` is td's, maps code only from the closure
    /// and runs without loader variables.
    fn td_process(&self, pid: u32, closure: &Closure) -> R<()> {
        require(
            fs::canonicalize(fmt(format_args!("/proc/{pid}/exe"))?)?
                == fs::canonicalize(closure.postgres()?)?,
            "PostgreSQL does not run the td-built server",
        )?;
        let maps =
            td_fs::read_bounded(Path::new(&fmt(format_args!("/proc/{pid}/maps"))?), 1 << 22)?;
        if let Some(path) = foreign_code(std::str::from_utf8(&maps)?, &closure.basenames()) {
            return Err(fmt(format_args!("td-built PostgreSQL maps code from {path}"))?.into());
        }
        let environ = td_fs::read_bounded(
            Path::new(&fmt(format_args!("/proc/{pid}/environ"))?),
            1 << 20,
        )?;
        require(
            !environ
                .split(|b| *b == 0)
                .any(|variable| variable.starts_with(b"LD_")),
            "td-built PostgreSQL runs with a loader variable",
        )
    }

    /// The drop-in that runs the td-built server with the arguments the unit
    /// gives the Debian one.
    fn binaries_dropin(&self, closure: &Closure) -> R<Setting> {
        let mut show = self.host.cmd("systemctl");
        show.args(["show", PG]);
        for property in UNIT_PROPERTIES {
            show.args(["-p", property]);
        }
        unit_state(&self.host.out(&mut show)?)?;
        let property = crate::readiness::exec_start(&self.host, PG_BUS)?;
        let debian = child(Path::new(DEBIAN_TOOLS), "postgres")?;
        let args = server_arguments(&property, debian.to_str().ok_or("Debian path")?)?;
        let program = closure.postgres()?;
        Ok(Setting {
            path: PathBuf::from(BINARIES_DROPIN),
            text: dropin_text(program.to_str().ok_or("store path")?, &args)?,
            mode: 0o644,
        })
    }

    // systemd's reloaded unit runs exactly what the drop-in says.
    fn check_unit(&self, closure: &Closure, text: &str) -> R<()> {
        let property = crate::readiness::exec_start(&self.host, PG_BUS)?;
        let program = closure.postgres()?;
        let program = program.to_str().ok_or("store path")?;
        require(
            dropin_text(program, &server_arguments(&property, program)?)? == text,
            "systemd does not run the td-built server as installed",
        )?;
        let unset = self.host.out(self.host.cmd("systemctl").args([
            "show",
            PG,
            "-p",
            "UnsetEnvironment",
            "--value",
        ]))?;
        require(
            UNSET_NAMES
                .iter()
                .all(|name| unset.split_ascii_whitespace().any(|v| v == *name)),
            "systemd keeps a loader variable for PostgreSQL",
        )
    }

    // Before writer admission: the Debian server on the same data directory.
    // The verified store paths stay; nothing runs them.
    fn restore_debian(&mut self, switch: &mut Switch) -> R<()> {
        if !self.fence_owned || self.admitting {
            self.host.select_tools(None);
            return Ok(());
        }
        self.event("restoring the Debian PostgreSQL before writer admission")?;
        if switch.stopped {
            self.host.exec(
                self.host.cmd("systemctl").args(["stop", PG]),
                &[],
                Duration::from_secs(120),
                true,
            )?;
            // What the td server, if it ran, left for the Debian one.
            let state = match self.control_state(Path::new(PGDATA)) {
                Ok(state) => state,
                Err(e) => fmt(format_args!("unreadable: {e}"))?,
            };
            set(
                &mut self.record,
                "postgres_td_control_state",
                Json::Str(state),
            )?;
            if let Some(setting) = &switch.dropin {
                files::rollback(std::iter::once(setting))?;
            }
            require(
                !Path::new(BINARIES_DROPIN).try_exists()?,
                "PostgreSQL binaries drop-in remains",
            )?;
            self.host.select_tools(None);
            self.host
                .run(self.host.cmd("systemctl").arg("daemon-reload"))?;
            self.host
                .run(self.host.cmd("systemctl").args(["start", PG]))?;
            self.wait(25, "Debian PostgreSQL restart", || self.ready_db(&[]))?;
            self.check_source()?;
            require(
                fs::canonicalize(fmt(format_args!("/proc/{}/exe", self.pid(PG)?))?)?
                    == fs::canonicalize(child(Path::new(DEBIAN_TOOLS), "postgres")?)?,
                "the restored server is not the Debian PostgreSQL",
            )?;
        }
        self.host.select_tools(None);
        if self.socket_fence {
            open_socket(&mut LiveSocket(&self.host))?;
            self.socket_fence = false;
        }
        self.config(&self.previous, &[], self.isolated)?;
        self.admit()?;
        self.wait(240, "restored ACME lease", || self.acme_online())?;
        self.healthy(&self.previous)?;
        set(
            &mut self.record,
            "rollback",
            "Debian PostgreSQL restored".into(),
        )
    }
}

#[cfg(test)]
#[path = "tests/binaries.rs"]
mod tests;
