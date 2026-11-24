//! Select a tested main revision and submit a pinned, standalone operator job.
use crate::{
    activate::{
        binaries::{dropin_program, BINARIES_DROPIN, DEBIAN_SOURCE},
        FENCE,
    },
    common::{
        banner_minor, child, digest, fmt, owned, postgres_minor, require, set, store_basename,
        Host, DEBIAN_LIBRARIES, DEBIAN_TOOLS,
    },
    probes::now,
    target::{HostConfig, Target, HOST_CONFIG},
    R,
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use td_json::Json;

#[path = "launcher_closure.rs"]
mod closure;
#[path = "launcher_converge.rs"]
mod converge;
#[path = "launcher_files.rs"]
mod files;
#[path = "launcher_host_key.rs"]
mod host_key;
#[path = "launcher_transport.rs"]
mod transport;

const RELEASES: &str = "/opt/td-mta/lib/td-mta/releases/git-";
const OPERATORS: &str = "/opt/td-mta/lib/td-mta/deploy";
/// A socket cutover that left its fence.
const SOCKET_LEFT: &str = "the socket cutover left the deployment fence, which --recover does not model: its receipt's socket_files and socket_originals name what it changed (td-mta/PRODUCTION.md, \"Leaving loopback TLS\")";
/// A stepping-stone job that ended unseen or left its fence.
const STEPPING_LEFT: &str = "the stepping-stone job ended unseen or left the deployment fence, and --recover does not model the loopback-TLS controller's jobs: td-mta/deploy reports whether it still runs or what its fence needs (td-mta/PRODUCTION.md, \"Leaving loopback TLS\")";
/// The refusal of an irreversible deployment that names no migration.
const UNNAMED: &str = "an irreversible deployment names its migration: pass --irreversible --schema-target N --revision HEX together";
/// The refusal when main no longer selects an authorised revision.
const MOVED: &str = "main no longer selects the revision --revision authorised: its migration needs its own decision";
const HELP: &str = "Usage: td-mta/deploy [--skip-tests] [OPTIONS]
       td-mta/deploy --postgres-socket [OPTIONS]
       td-mta/deploy --postgres-binaries [--to-debian] [OPTIONS]
       td-mta/deploy --retire-rollback --irreversible [OPTIONS]
       td-mta/deploy --recover [OPTIONS]
       td-mta/deploy --rollback [OPTIONS]
       td-mta/deploy --restore-shared-services [OPTIONS]
       td-mta/deploy --refresh-notifications [--config FILE]
       td-mta/deploy --converge-config [--plan | --expect-digest HEX] [OPTIONS]
       td-mta/deploy --write-target --host HOST --host-name NAME --account ACCOUNT
         --identity KEY --jmap-account ACCOUNT --jmap-url URL
         [--host-key-sha256 SHA256:FP] [--config FILE] [--replace]
       td-mta/deploy --provision-host --build-account ACCOUNT --build-home DIR
         --public-name NAME [--config FILE] [--replace]
Options: [--window-seconds 1..3600] [--jmap-account ACCOUNT] [--config FILE]
  [--irreversible --schema-target N --revision HEX] (required by a
  deployment that migrates the database: authorises the migration of
  that revision to schema N, as the refusal names it; a stepping stone
  takes --irreversible --revision HEX naming the stone)
Select a verified origin/main revision and deploy it to the configured host,
move the live release's database from loopback TLS to its private socket, or
switch it from the Debian PostgreSQL or another td closure to td's
source-built one, applying what the reviewed minor-release table says each
release in between needs; --to-debian returns it to the retained Debian one.
--retire-rollback deletes rollback material older than seven days: earlier
job directories, physical recovery copies, and the Debian PostgreSQL tree
once a verified switch left it. --recover finishes or rolls back a job
whose controller stopped inside its writer fence, as that controller
would have, keeping the live release otherwise. --rollback reactivates
the release replaced by the activation that installed the live one,
when that activation migrated nothing. --restore-shared-services returns
from the identity cutover that installed the live release to the shared
service identities; --refresh-notifications rederives the isolated
notification configuration from the managed config.json.
--converge-config converges the selected configuration to the live release's
defaults plus its td-mta/production-overrides.conf; --plan only reports, and
--expect-digest applies only the configuration a plan reported.
While PostgreSQL still uses loopback TLS and the live release predates
group-socket admission, a deploy installs the newest verified stepping
stone (td-mta/PRODUCTION.md, \"Leaving loopback TLS\") with the
loopback-TLS controller; it needs --irreversible --revision naming the
stone, and --skip-tests when no
stepping stone has a verdict. Every other operation there refuses and
names the step that comes next.
Every command ends with a td-mta-deploy-outcome JSON line.
The target is FILE, else $XDG_CONFIG_HOME/td-mta-deploy/target.json, else
~/.config/td-mta-deploy/target.json (td-mta/PRODUCTION.md). --write-target
writes that file; --provision-host then prepares the host's build account
and installs its /var/lib/td-mta-operator/host.json. Either refuses to
change an existing file that says something else unless --replace.
--host-key-sha256 enrolls the host key with that fingerprint, obtained
over another channel, in FILE.known_hosts, which every SSH connection
then trusts alone.";
struct Options<'a> {
    skip: bool,
    budget: u64,
    account: Option<&'a str>,
    config: Option<&'a str>,
    window: bool,
    socket: bool,
    binaries: bool,
    to_debian: bool,
    retire: bool,
    recover: bool,
    rollback: bool,
    restore: bool,
    refresh: bool,
    irreversible: bool,
    converge: bool,
    plan: bool,
    expect: Option<&'a str>,
    /// The schema version an irreversible deployment may migrate to.
    schema_target: Option<u64>,
    /// The revision an irreversible deployment authorises.
    revision: Option<&'a str>,
}
fn parse(args: &[String]) -> R<Options<'_>> {
    let mut out = Options {
        skip: false,
        budget: 60,
        account: None,
        config: None,
        window: false,
        socket: false,
        binaries: false,
        to_debian: false,
        retire: false,
        recover: false,
        rollback: false,
        restore: false,
        refresh: false,
        irreversible: false,
        converge: false,
        plan: false,
        expect: None,
        schema_target: None,
        revision: None,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--skip-tests" => out.skip = true,
            "--window-seconds" => {
                out.budget = it.next().ok_or("missing window")?.parse()?;
                out.window = true;
                require(
                    (1..=3600).contains(&out.budget),
                    "window must be 1..3600 seconds",
                )?;
            }
            "--jmap-account" => out.account = Some(it.next().ok_or("missing JMAP account")?),
            "--config" => out.config = Some(it.next().ok_or("missing target configuration")?),
            "--postgres-socket" => out.socket = true,
            "--postgres-binaries" => out.binaries = true,
            "--to-debian" => out.to_debian = true,
            "--retire-rollback" => out.retire = true,
            "--recover" => out.recover = true,
            "--rollback" => out.rollback = true,
            "--restore-shared-services" => out.restore = true,
            "--refresh-notifications" => out.refresh = true,
            "--irreversible" => out.irreversible = true,
            "--converge-config" => out.converge = true,
            "--plan" => out.plan = true,
            "--expect-digest" => {
                let digest = it.next().ok_or("missing expected digest")?;
                require(
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
                    "invalid expected digest",
                )?;
                out.expect = Some(digest);
            }
            "--schema-target" => {
                let target = it.next().ok_or("missing schema target")?;
                require(
                    !target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()),
                    "invalid schema target",
                )?;
                out.schema_target = Some(target.parse().map_err(|_| "invalid schema target")?);
            }
            "--revision" => {
                let revision = it.next().ok_or("missing revision")?;
                require(
                    revision.len() == 40
                        && revision
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                    "--revision takes the 40-hex revision the refusal names",
                )?;
                out.revision = Some(revision);
            }
            other => {
                return Err(fmt(format_args!(
                    "unknown option {other}: td-mta/deploy --help lists them"
                ))?
                .into())
            }
        }
    }
    require(
        !((out.socket || out.binaries) && out.skip),
        "a PostgreSQL operation keeps the live release and selects no revision",
    )?;
    require(
        !(out.socket && out.binaries),
        "one PostgreSQL operation per job",
    )?;
    require(
        out.binaries || !out.to_debian,
        "--to-debian belongs to --postgres-binaries",
    )?;
    require(
        !(out.retire && (out.socket || out.binaries || out.skip || out.window)),
        "retiring rollback material is its own job and has no writer window",
    )?;
    require(
        !(out.recover
            && (out.socket
                || out.binaries
                || out.skip
                || out.window
                || out.retire
                || out.irreversible)),
        "recovery is its own job: it takes no other operation",
    )?;
    require(
        !(out.rollback
            && (out.socket
                || out.binaries
                || out.skip
                || out.retire
                || out.recover
                || out.irreversible)),
        "a rollback is its own binary-only activation",
    )?;
    let others = out.socket
        || out.binaries
        || out.skip
        || out.retire
        || out.recover
        || out.rollback
        || out.irreversible
        || out.converge;
    require(
        !(out.restore && (others || out.refresh)),
        "restoring the shared service identities is its own job",
    )?;
    require(
        !(out.refresh && (others || out.window || out.account.is_some())),
        "refreshing notifications is its own job and touches no service",
    )?;
    require(
        out.irreversible || !out.retire,
        "retiring rollback material cannot be undone: pass --irreversible",
    )?;
    require(
        !(out.irreversible && (out.socket || out.binaries)),
        "--irreversible applies to a deployment or --retire-rollback",
    )?;
    require(
        !(out.converge
            && (out.skip
                || out.socket
                || out.binaries
                || out.retire
                || out.recover
                || out.rollback
                || out.irreversible
                || out.window)),
        "configuration convergence keeps the live release, is its own job and has no writer window",
    )?;
    require(
        out.converge || !(out.plan || out.expect.is_some()),
        "--plan and --expect-digest apply to --converge-config",
    )?;
    require(
        !(out.plan && out.expect.is_some()),
        "--expect-digest binds an apply to a plan",
    )?;
    let deploying = out.irreversible && !out.retire;
    // A stepping stone's controller reads no schema target; `deploy`
    // requires one for every other deployment once it knows which.
    require(
        deploying == out.revision.is_some() && (deploying || out.schema_target.is_none()),
        UNNAMED,
    )?;
    Ok(out)
}
/// A deployment's authorisation names its migration's schema target,
/// except a stepping stone's, whose controller reads none.
fn named_migration(opts: &Options<'_>, stepping: bool) -> R<()> {
    require(
        stepping || !opts.irreversible || opts.retire || opts.schema_target.is_some(),
        UNNAMED,
    )
}
/// A file's text as the remote command printed it, which loses its final
/// newline.
fn remote_text(output: &str) -> R<String> {
    let mut text = owned(output.trim_end_matches('\n'))?;
    td_alloc::push_str(&mut text, "\n")?;
    Ok(text)
}
fn revision(value: &str) -> R<&str> {
    require(
        value.len() == 40
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        "invalid Git revision",
    )?;
    Ok(value)
}
fn advertised(text: &str) -> R<&str> {
    let mut fields = text.split_whitespace();
    let v = revision(fields.next().ok_or("missing main")?)?;
    require(
        fields.next() == Some("refs/heads/main") && fields.next().is_none(),
        "unexpected main advertisement",
    )?;
    Ok(v)
}
fn live_path(path: &str) -> R<&str> {
    revision(
        path.strip_prefix(RELEASES)
            .and_then(|v| v.strip_suffix("/td-mta"))
            .ok_or("unrecognized live release")?,
    )
}
fn verdict(path: &Path) -> R<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(path)?;
    let mut f = text.split_whitespace();
    let (Some("pass"), Some("0"), Some(a), Some(b), None) =
        (f.next(), f.next(), f.next(), f.next(), f.next())
    else {
        return Ok(None);
    };
    if [a, b].iter().all(|s| s.bytes().all(|c| c.is_ascii_digit())) {
        Ok(Some(fmt(format_args!("pass 0 {a} {b}"))?))
    } else {
        Ok(None)
    }
}
fn quote(text: &str, out: &mut String) -> R<()> {
    td_alloc::push_str(out, "'")?;
    for (i, segment) in text.split('\'').enumerate() {
        if i > 0 {
            td_alloc::push_str(out, "'\\''")?;
        }
        td_alloc::push_str(out, segment)?;
    }
    td_alloc::push_str(out, "'")?;
    Ok(())
}
fn words(parts: &[&str]) -> R<String> {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i != 0 {
            td_alloc::push_str(&mut out, " ")?;
        }
        quote(part, &mut out)?;
    }
    Ok(out)
}
struct Local {
    host: Host,
    root: PathBuf,
    destination: String,
    key: PathBuf,
    /// Trust exactly the enrolled host key, when there is one.
    ssh_options: Vec<String>,
}
impl Local {
    fn git(&self) -> Command {
        let mut c = Command::new("git");
        c.current_dir(&self.root);
        c
    }
    fn ssh(&self, args: &[&str]) -> R<Command> {
        let mut c = Command::new("ssh");
        c.args([
            "-T",
            "-oConnectTimeout=10",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oIdentitiesOnly=yes",
        ])
        .args(&self.ssh_options)
        .arg("-i")
        .arg(&self.key)
        .arg(&self.destination)
        .arg(words(args)?);
        Ok(c)
    }
    fn remote(&self, args: &[&str]) -> R<String> {
        self.host.out(&mut self.ssh(args)?)
    }
    fn sudo(&self, args: &[&str]) -> R<String> {
        let mut command = Vec::new();
        td_alloc::extend_from_slice(&mut command, &["sudo", "-n"])?;
        td_alloc::extend_from_slice(&mut command, args)?;
        self.remote(&command)
    }
    fn is_ancestor(&self, earlier: &str, later: &str) -> R<bool> {
        let status = self
            .host
            .exec(
                self.git()
                    .args(["merge-base", "--is-ancestor", earlier, later]),
                &[],
                Duration::from_secs(30),
                false,
            )?
            .status;
        match status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err("git ancestry lookup failed".into()),
        }
    }
    fn origin_revisions(&self) -> R<(String, String)> {
        let ad = self.host.out(self.git().args([
            "ls-remote",
            "--exit-code",
            "--refs",
            "origin",
            "refs/heads/main",
        ]))?;
        let tip = owned(advertised(&ad)?)?;
        self.host.run(self.git().args([
            "fetch",
            "--no-write-fetch-head",
            "--no-tags",
            "origin",
            &tip,
        ]))?;
        let controller = self
            .host
            .out(self.git().args(["rev-parse", "HEAD^{commit}"]))?;
        revision(&controller)?;
        self.host.run(self.git().args([
            "diff",
            "--quiet",
            &controller,
            "--",
            "td-mta/deploy",
            "td-mta/src/bin/td-mta-deploy",
            "td-mta/Cargo.toml",
            "td-mta/Cargo.lock",
        ]))?;
        let submitted = self.host.out(self.git().args([
            "for-each-ref",
            "--contains",
            &controller,
            "--format=%(refname)",
            "refs/remotes/origin/",
        ]))?;
        require(
            !submitted.is_empty() || self.is_ancestor(&controller, &tip)?,
            "controller revision has not been submitted",
        )?;
        Ok((tip, controller))
    }
    fn select(
        &self,
        tip: &str,
        live: &str,
        skip: bool,
        state: &Path,
    ) -> R<(String, Option<String>)> {
        require(
            self.is_ancestor(live, tip)?,
            "live release is not an ancestor of advertised main",
        )?;
        if skip {
            println!("Skipping the main-integration verdict check for fetched origin/main.");
            return Ok((owned(tip)?, None));
        }
        if let Some((rev, row)) = self.verified_after(tip, live, state)? {
            return Ok((rev, Some(row)));
        }
        if live != tip {
            println!("No newer landed revision has a passing main-integration verdict. Checking the deployed release.");
        }
        Ok((owned(live)?, None))
    }
    /// The newest first-parent descendant of `live` up to `tip` with a
    /// passing verdict in `state`, and that verdict.
    fn verified_after(&self, tip: &str, live: &str, state: &Path) -> R<Option<(String, String)>> {
        let revs = self.host.out(self.git().args([
            "rev-list",
            "--first-parent",
            tip,
            &fmt(format_args!("^{live}"))?,
        ]))?;
        for rev in revs.lines() {
            revision(rev)?;
            if self.is_ancestor(live, rev)? {
                if let Some(row) = verdict(&child(state, rev)?)? {
                    return Ok(Some((owned(rev)?, row)));
                }
            }
        }
        Ok(None)
    }
    fn save(&self, path: &str, value: &Json) -> R<()> {
        self.save_bytes(path, fmt(format_args!("{value}"))?.as_bytes())
    }
    /// Replace `path` on the host with a root-owned mode-0600 file.
    fn save_bytes(&self, path: &str, bytes: &[u8]) -> R<()> {
        let tmp = fmt(format_args!("{path}.pending"))?;
        self.host.exec(
            &mut self.ssh(&["sudo", "-n", "install", "-m", "0600", "/dev/stdin", &tmp])?,
            bytes,
            Duration::from_secs(30),
            true,
        )?;
        self.sudo(&["mv", "-T", &tmp, path])?;
        self.sudo(&["sync", "-f", path])?;
        Ok(())
    }
    /// The exit status of `args` on the host, run as the login account.
    fn remote_status(&self, args: &[&str]) -> R<Option<i32>> {
        Ok(self
            .host
            .exec(&mut self.ssh(args)?, &[], Duration::from_secs(40), false)?
            .status
            .code())
    }
    fn upload(&self, file: &Path, dst: &str, mode: &str) -> R<()> {
        let mut c = self.ssh(&["sudo", "-n", "install", "-m", mode, "/dev/stdin", dst])?;
        let out = c.stdin(File::open(file)?).stdout(Stdio::null()).status()?;
        require(out.success(), "upload failed")
    }
    // Into the root-only job directory, checked there.
    fn send_nar(&self, built: &closure::Built, job: &str) -> R<()> {
        let dst = fmt(format_args!("{job}/{}.nar", built.basename))?;
        self.upload(&built.nar, &dst, "0600")?;
        require(
            self.sudo(&["sha256sum", "--", &dst])?
                .split_whitespace()
                .next()
                == Some(&built.sha256),
            "uploaded NAR mismatch",
        )
    }
    // Early and read-only, before a build of up to hours: the live server,
    // `debian` or the td closure's PostgreSQL output, and its release,
    // which the minor-release table must list.
    fn live_release(&self) -> R<(String, u32)> {
        // Found whether or not its directory exists.
        let found = self.sudo(&[
            "find",
            "/etc/systemd/system",
            "-maxdepth",
            "2",
            "-path",
            BINARIES_DROPIN,
            "-type",
            "f",
        ])?;
        if found.trim().is_empty() {
            return Ok((owned(DEBIAN_SOURCE)?, self.debian_release()?));
        }
        let text = remote_text(&self.sudo(&["cat", "--", BINARIES_DROPIN])?)?;
        let program = dropin_program(&text)?;
        let banner = self.sudo(&["env", "-i", program, "--version"])?;
        let minor = banner_minor(&banner)?;
        crate::postgres_minors::covered(minor)?;
        let basename = program
            .strip_prefix("/td/store/")
            .and_then(|rest| rest.split_once('/'))
            .map(|(name, _)| name)
            .ok_or("the PostgreSQL binaries drop-in names no td-built server")?;
        Ok((owned(basename)?, minor))
    }
    // The retained Debian release, which the minor-release table must list.
    fn debian_release(&self) -> R<u32> {
        let library = fmt(format_args!("LD_LIBRARY_PATH={DEBIAN_LIBRARIES}"))?;
        let server = child(Path::new(DEBIAN_TOOLS), "postgres")?;
        let banner = self.sudo(&[
            "env",
            "-i",
            &library,
            server.to_str().ok_or("Debian path")?,
            "--version",
        ])?;
        let minor = banner_minor(&banner)?;
        crate::postgres_minors::covered(minor)?;
        Ok(minor)
    }
    fn cargo(&self, checkout: &Path) -> R<Command> {
        let mut build = Command::new("cargo");
        build.current_dir(checkout);
        // The Guix seed puts its host GCC runtime outside the dynamic loader's
        // search path. Cargo's build scripts and proc macros run on that host.
        if Path::new("/gnu/store").is_dir() {
            let runtime = self
                .host
                .out(Command::new("cc").arg("-print-file-name=libgcc_s.so.1"))?;
            let path = Path::new(&runtime);
            if path.is_absolute() && path.is_file() {
                let mut directory = fs::canonicalize(path)?;
                require(directory.pop(), "compiler runtime directory")?;
                let previous = std::env::var_os("LD_LIBRARY_PATH");
                let inherited = previous.iter().flat_map(std::env::split_paths);
                build.env(
                    "LD_LIBRARY_PATH",
                    std::env::join_paths(std::iter::once(directory).chain(inherited))?,
                );
            }
        }
        Ok(build)
    }
    fn artifact(&self, temp: &Path, controller: &str) -> R<PathBuf> {
        let checkout = child(temp, "source")?;
        self.host.exec(
            self.git()
                .args(["clone", "--quiet", "--shared", "--no-checkout", "--"])
                .arg(&self.root)
                .arg(&checkout),
            &[],
            Duration::from_secs(150),
            true,
        )?;
        self.host.exec(
            self.git()
                .arg("-C")
                .arg(&checkout)
                .args(["checkout", "--quiet", "--detach", controller]),
            &[],
            Duration::from_secs(150),
            true,
        )?;
        let target = child(temp, "target")?;
        let mut build = self.cargo(&checkout)?;
        build
            .args(["build","--frozen","--release","--target","x86_64-unknown-linux-musl","--bin","td-mta-deploy","--manifest-path","td-mta/Cargo.toml","--target-dir"]).arg(&target)
            .env("CARGO_ENCODED_RUSTFLAGS",fmt(format_args!("-Clinker=rust-lld\x1f-Ctarget-feature=+crt-static\x1f--remap-path-prefix={}=/td-deploy-source",checkout.display()))?);
        require(
            build.status()?.success(),
            "building pinned static controller failed",
        )?;
        let bin = child(&target, "x86_64-unknown-linux-musl/release/td-mta-deploy")?;
        let report = self.host.out(
            Command::new("readelf")
                .env("LC_ALL", "C")
                .args(["-h", "-l", "-d", "-W"])
                .arg(&bin),
        )?;
        require(
            report.contains("Advanced Micro Devices X86-64")
                && !report.contains("INTERP")
                && !report.contains("(NEEDED)"),
            "operator requires a static x86-64 Linux executable",
        )?;
        Ok(bin)
    }
    fn install(&self, bin: &Path, controller: &str, job: &str) -> R<(String, String)> {
        let hash = digest(bin)?;
        let dir = fmt(format_args!("{OPERATORS}/{controller}/{hash}"))?;
        let installed = fmt(format_args!("{dir}/td-mta-deploy"))?;
        self.sudo(&["install", "-d", "-m", "0755", &dir])?;
        let jobname = job.rsplit('/').next().ok_or("job name")?;
        let stage = fmt(format_args!("{dir}/.{jobname}"))?;
        self.upload(bin, &stage, "0755")?;
        require(
            self.sudo(&["sha256sum", "--", &stage])?
                .split_whitespace()
                .next()
                == Some(&hash),
            "uploaded controller mismatch",
        )?;
        // A previously published content address is immutable, even during retries.
        let publish = self.host.exec(
            &mut self.ssh(&["sudo", "-n", "ln", "-T", "--", &stage, &installed])?,
            &[],
            Duration::from_secs(30),
            false,
        )?;
        self.sudo(&["rm", "--", &stage])?;
        if !publish.status.success() {
            require(
                self.sudo(&["stat", "-c", "%u:%g:%a:%F", "--", &installed])?
                    == "0:0:755:regular file",
                "invalid retained controller",
            )?;
        }
        require(
            self.sudo(&["sha256sum", "--", &installed])?
                .split_whitespace()
                .next()
                == Some(&hash),
            "retained controller digest",
        )?;
        self.sudo(&["sync", "-f", &installed])?;
        self.sudo(&[
            "ln",
            "-s",
            "--",
            &installed,
            &fmt(format_args!("{job}/td-mta-deploy"))?,
        ])?;
        Ok((installed, hash))
    }
    fn qualify_jmap(&self, account: &str, url: &str) -> R<f64> {
        fn request(command: &str, account: Option<&str>) -> R<Json> {
            let mut j = Json::Obj(Vec::new());
            set(&mut j, "command", Json::Str(owned(command)?))?;
            if let Some(a) = account {
                set(&mut j, "account", Json::Str(owned(a)?))?;
            }
            Ok(j)
        }
        let invoke = |requests: &[Json]| -> R<Vec<Json>> {
            let mut input = String::new();
            for j in requests {
                td_alloc::push_str(&mut input, &fmt(format_args!("{j}\n"))?)?;
            }
            let p = self.host.exec(
                Command::new("td-mail").current_dir(&self.root).arg("--cli"),
                input.as_bytes(),
                Duration::from_secs(30),
                true,
            )?;
            let mut rows = Vec::new();
            for line in std::str::from_utf8(&p.stdout)?
                .lines()
                .filter(|s| s.starts_with('{'))
            {
                let row = td_json::parse(line)?;
                require(
                    row.get("ok").and_then(Json::as_bool) == Some(true),
                    "public JMAP check failed",
                )?;
                td_alloc::push(&mut rows, row)?;
            }
            require(rows.len() == requests.len(), "missing JMAP response")?;
            Ok(rows)
        };
        let first = invoke(&[request("list_accounts", None)?])?;
        let accounts = first
            .first()
            .and_then(|r| r.get("accounts").and_then(Json::as_arr))
            .ok_or("JMAP accounts")?;
        require(
            accounts.iter().any(|a| {
                a.get("name").and_then(Json::as_str) == Some(account)
                    && a.get("well_known_url").and_then(Json::as_str) == Some(url)
            }),
            "production discovery URL required",
        )?;
        let rows = invoke(&[
            request("connect", Some(account))?,
            request("list_mailboxes", None)?,
        ])?;
        require(
            rows.last()
                .and_then(|v| v.get("mailboxes").and_then(Json::as_arr))
                .is_some_and(|m| !m.is_empty()),
            "public JMAP mailbox qualification",
        )?;
        now()
    }
}
/// The operator's home (absolute), the target configuration `config` or
/// the default path names, and a session to the host it names.
fn session(config: Option<&str>) -> R<(Local, Target, PathBuf)> {
    let host = Host::default();
    let root = PathBuf::from(host.out(Command::new("git").args(["rev-parse", "--show-toplevel"]))?);
    let home = operator_home()?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let target = Target::load(config.map(Path::new), &home, xdg.as_deref())?;
    let path = crate::target::target_path(config.map(Path::new), &home, xdg.as_deref())?;
    let ssh_options = host_key::ssh_options(&path)?;
    let local = Local {
        ssh_options,
        host,
        root,
        destination: owned(&target.destination)?,
        key: PathBuf::from(owned(
            target
                .identity
                .to_str()
                .ok_or("deploy target: invalid identity")?,
        )?),
    };
    Ok((local, target, home))
}
fn operator_home() -> R<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("operator home")?);
    require(home.is_absolute(), "operator home must be absolute")?;
    Ok(home)
}
/// The command that follows: its arguments, and the options whose values
/// the operator supplies.
struct Next<'a> {
    argv: &'a [&'a str],
    needs: &'a [&'a str],
}
struct Outcome<'a> {
    result: &'a str,
    /// `--write-target --host-key-sha256`: what happened to known_hosts.
    host_key: Option<&'a str>,
    error: Option<&'a str>,
    /// Further members, in order, from a JSON object; none may be named
    /// `result`, `host_key`, `error` or `next`.
    details: Option<&'a Json>,
    next: Option<Next<'a>>,
}
fn strings(list: &[&str]) -> R<Json> {
    let mut out = Vec::new();
    for item in list {
        td_alloc::push(&mut out, Json::Str(owned(item)?))?;
    }
    Ok(Json::Arr(out))
}
/// `td-mta-deploy-outcome` and a JSON object: what happened, the refusal
/// if any, and the command that follows, if any.
fn outcome_line(outcome: &Outcome<'_>) -> R<String> {
    let mut line = Json::Obj(Vec::new());
    set(&mut line, "result", Json::Str(owned(outcome.result)?))?;
    if let Some(host_key) = outcome.host_key {
        set(&mut line, "host_key", Json::Str(owned(host_key)?))?;
    }
    if let Some(error) = outcome.error {
        set(&mut line, "error", Json::Str(owned(error)?))?;
    }
    if let Some(details) = outcome.details {
        let Json::Obj(members) = details else {
            return Err("outcome details must be a JSON object".into());
        };
        for (key, value) in members {
            require(
                !["result", "host_key", "error", "next"].contains(&key.as_str()),
                "outcome details reuse a member name",
            )?;
            set(&mut line, key, crate::common::copy_json(value)?)?;
        }
    }
    let next = match &outcome.next {
        Some(next) => {
            let mut object = Json::Obj(Vec::new());
            set(&mut object, "argv", strings(next.argv)?)?;
            set(&mut object, "needs", strings(next.needs)?)?;
            object
        }
        None => Json::Null,
    };
    set(&mut line, "next", next)?;
    fmt(format_args!("td-mta-deploy-outcome {line}\n"))
}
/// The command's final stdout line, for whoever runs it next.
fn outcome(outcome: &Outcome<'_>) -> R<()> {
    write_outcome(&mut std::io::stdout().lock(), outcome)
}
fn write_outcome(out: &mut impl std::io::Write, outcome: &Outcome<'_>) -> R<()> {
    out.write_all(outcome_line(outcome)?.as_bytes())?;
    out.flush()?;
    Ok(())
}
pub(super) fn run(args: &[String]) -> R<()> {
    if matches!(args,[s] if s=="--help"||s=="-h") {
        println!("{HELP}");
        return Ok(());
    }
    match args.split_first() {
        Some((first, rest)) if first == "--write-target" => return files::write_target(rest),
        Some((first, rest)) if first == "--provision-host" => return files::provision_host(rest),
        _ => (),
    }
    let mut session = converge::Session {
        again: again(args, &[])?,
        ..converge::Session::default()
    };
    let result = parse(args).and_then(|opts| match deploy(&opts, args, &mut session)? {
        Some(job) => Ok(Some((ended(&opts, args, &job)?, job))),
        None => Ok(None),
    });
    let (end, job) = match result {
        // A convergence reports its own outcome.
        Ok(None) => return Ok(()),
        Ok(Some(done)) => done,
        Err(error) => {
            let mut out = std::io::stdout().lock();
            // A refusal naming what follows is this run's one line, for
            // every mode; any other error of a convergence is its own.
            let (message, next) = match error.downcast::<Refusal>() {
                Ok(refusal) => (refusal.error, refusal.next),
                Err(error) => {
                    let message = fmt(format_args!("{error}"))?;
                    if args.iter().any(|arg| arg == "--converge-config") {
                        converge::unreported(
                            &mut out,
                            &mut session,
                            &message,
                            &converge::carried(args)?,
                        )?;
                        return Err(error);
                    }
                    let next = refused_next(&message, args)?;
                    (message, next)
                }
            };
            let end = refused_end(message, next, args)?;
            write_ended(&mut out, &end, Json::Obj(Vec::new()))?;
            return Err(end.error.unwrap_or_default().into());
        }
    };
    write_job_outcome(&mut std::io::stdout().lock(), &end, &job)?;
    match end.error {
        None => Ok(()),
        Some(error) => Err(error.into()),
    }
}

/// A job's outcome line: the shared fields, with the job's last status,
/// receipt path and, for retirement, its next time as details.
fn write_job_outcome(out: &mut impl std::io::Write, end: &Ended, job: &Job) -> R<()> {
    let mut details = Json::Obj(Vec::new());
    if let Some(status) = job
        .record
        .as_ref()
        .and_then(|r| r.get("status"))
        .and_then(Json::as_str)
    {
        set(&mut details, "status", Json::Str(owned(status)?))?;
    }
    // Only a record read names where it came from.
    if job.record.is_some() {
        set(&mut details, "receipt", Json::Str(owned(&job.receipt)?))?;
    }
    if let Some(time) = end.not_before {
        set(&mut details, "not_before", Json::from(time))?;
    }
    write_ended(out, end, details)
}

/// The shared outcome line of `end`, with `details` (a JSON object) and,
/// for a decision, `approve` among them.
fn write_ended(out: &mut impl std::io::Write, end: &Ended, mut details: Json) -> R<()> {
    if let Some((argv, needs)) = &end.approve {
        set(&mut details, "approve", command_json(argv, needs)?)?;
    }
    let mut words = Vec::new();
    for word in end.next.iter().flatten() {
        td_alloc::push(&mut words, word.as_str())?;
    }
    write_outcome(
        out,
        &Outcome {
            result: end.result,
            host_key: None,
            error: end.error.as_deref(),
            details: Some(&details),
            next: end.next.as_ref().map(|_| Next {
                argv: &words,
                needs: &[],
            }),
        },
    )
}

/// Everything up to and including the job: refusals before it are the
/// caller's to report. A convergence reports its own end and gives None.
fn deploy(opts: &Options<'_>, args: &[String], state: &mut converge::Session) -> R<Option<Job>> {
    let (local, target, home) = session(opts.config)?;
    let account = opts.account.unwrap_or(&target.jmap_account);
    local.login()?;
    // The host's own description names its build mirror and identity.
    let site = local.sudo(&["cat", "--", HOST_CONFIG]).map_err(|_| {
        "cannot read /var/lib/td-mta-operator/host.json over SSH and sudo: provision it with td-mta/deploy --provision-host (td-mta/PRODUCTION.md, \"Deploy target configuration\")"
    })?;
    let site = HostConfig::parse(site.as_bytes())?;
    require(
        site.host_name == target.host_name,
        "the host's host.json names another host than target.json's host_name",
    )?;
    let (tip, controller) = local.origin_revisions()?;
    let live = local.remote(&["readlink", "-f", "/opt/td-mta/bin/td-mta"])?;
    let live = live_path(&live)?;
    let base = match std::env::var_os("TD_MAIN_INTEGRATION_DIR") {
        Some(p) => PathBuf::from(p),
        None => child(&home, ".local/state/td/main-integration")?,
    };
    let verdicts = child(&base, "verdicts")?;
    let stone = transport::plan(&local, opts, &site, &tip, live, &verdicts)?;
    let stepping = stone.is_some();
    named_migration(opts, stepping)?;
    let controller = if stepping {
        owned(transport::TLS_CONTROLLER)?
    } else {
        controller
    };
    let (selected, verdict) = if let Some(stone) = stone {
        stone
    } else if opts.socket
        || opts.binaries
        || opts.retire
        || opts.recover
        || opts.rollback
        || opts.converge
        || opts.restore
        || opts.refresh
    {
        require(
            local.is_ancestor(live, &tip)?,
            "live release is not an ancestor of advertised main",
        )?;
        (owned(live)?, None)
    } else {
        local.select(&tip, live, opts.skip, &verdicts)?
    };
    // An authorisation covers the revision it was given for, never one
    // main moved to since.
    require(opts.revision.is_none_or(|r| r == selected), MOVED)?;
    let tree = local.host.out(
        local
            .git()
            .args(["rev-parse", &fmt(format_args!("{selected}^{{tree}}"))?]),
    )?;
    revision(&tree)?;
    if opts.rollback {
        println!(
            "{} current {live}: the controller rolls back to the release it replaced",
            site.host_name
        );
    } else {
        println!("{} current {live}, selected {selected}", site.host_name);
    }
    let live_server = if opts.binaries {
        Some(local.live_release()?)
    } else {
        None
    };
    let debian_minor = if opts.to_debian {
        Some(local.debian_release().map_err(|e| {
            fmt(format_args!(
                "the retained Debian PostgreSQL did not report its release ({e}); once its tree is gone the server moves only between td closures"
            ))
            .map_or(e, Into::into)
        })?)
    } else {
        None
    };
    let policy = if opts.converge {
        Some(converge::policy(&local, live)?)
    } else {
        None
    };
    let cache = child(&local.root, ".td-build-cache")?;
    fs::create_dir_all(&cache)?;
    let scratch = td_fs::TempDir::new_in(&cache, "mta-deploy")?;
    let binary = local.artifact(scratch.path(), &controller);
    let binary = if stepping {
        transport::built(binary)?
    } else {
        binary?
    };
    let closure = match &live_server {
        Some((_, live_minor)) if !opts.to_debian => {
            let live_minor = *live_minor;
            let built = local.closure(scratch.path())?;
            let server = built.first().ok_or("missing PostgreSQL output")?;
            let td_minor = postgres_minor(store_basename(&server.basename, "postgresql-x86-64")?)?;
            // Before anything reaches the host: the table covers the move.
            crate::postgres_minors::movable(live_minor, td_minor)?;
            Some(built)
        }
        _ => None,
    };
    // A held fence keeps the services, and so public JMAP, down; a
    // rollback may be leaving a failing release.
    let before = if opts.recover || opts.refresh {
        None
    } else if opts.rollback || opts.restore {
        local.qualify_jmap(account, &target.jmap_url).ok()
    } else {
        Some(local.qualify_jmap(account, &target.jmap_url)?)
    };
    if selected != live {
        let key = local.key.to_str().ok_or("invalid SSH key path")?;
        let mirror = site.mirror()?;
        let remote = fmt(format_args!(
            "{}:{}",
            local.destination,
            mirror.to_str().ok_or("invalid source mirror")?
        ))?;
        let mut parts = Vec::new();
        td_alloc::extend_from_slice(
            &mut parts,
            &[
                "ssh",
                "-T",
                "-oConnectTimeout=10",
                "-oBatchMode=yes",
                "-oStrictHostKeyChecking=yes",
                "-oIdentitiesOnly=yes",
            ],
        )?;
        for option in &local.ssh_options {
            td_alloc::push(&mut parts, option.as_str())?;
        }
        td_alloc::extend_from_slice(&mut parts, &["-i", key])?;
        let ssh = words(&parts)?;
        local.host.exec(
            local.git().env("GIT_SSH_COMMAND", ssh).args([
                "push",
                "--no-follow-tags",
                &remote,
                &fmt(format_args!("{selected}:refs/td-mta-deploy/{selected}"))?,
            ]),
            &[],
            Duration::from_secs(300),
            true,
        )?;
    }
    let job = local.sudo(&["mktemp", "-d", "/var/lib/td-mta-operator/deploy-XXXXXXXX"])?;
    let token = job
        .strip_prefix("/var/lib/td-mta-operator/deploy-")
        .ok_or("unexpected operator job")?;
    require(
        token.len() == 8 && token.bytes().all(|b| b.is_ascii_alphanumeric()),
        "invalid operator job",
    )?;
    let (program, hash) = local.install(&binary, &controller, &job)?;
    for built in closure.iter() {
        for built in built {
            local.send_nar(built, &job)?;
        }
    }
    let policy_sha256 = match &policy {
        Some(policy) => {
            let hash = crate::common::digest_reader(&mut policy.as_slice())?;
            let destination = fmt(format_args!("{job}/overrides.conf"))?;
            local.save_bytes(&destination, policy)?;
            require(
                local
                    .sudo(&["sha256sum", "--", &destination])?
                    .split_whitespace()
                    .next()
                    == Some(&hash),
                "uploaded configuration policy mismatch",
            )?;
            Some(hash)
        }
        None => None,
    };
    // A stepping-stone job's spec is read by TLS_CONTROLLER: its load_job
    // takes at most 4096 bytes and Deployment::new reads revision,
    // previous, tree, main, controller and budget, keeping the rest.
    let mut spec = Json::Obj(Vec::new());
    for (k, v) in [
        ("previous", live),
        ("revision", selected.as_str()),
        ("tree", &tree),
        ("controller", &controller),
        ("main", tip.as_str()),
        ("controller_sha256", &hash),
    ] {
        set(&mut spec, k, Json::Str(owned(v)?))?;
    }
    set(&mut spec, "skip_tests", Json::Bool(opts.skip))?;
    if opts.socket {
        set(&mut spec, "mode", Json::Str(owned("postgres-socket")?))?;
    }
    if opts.retire {
        set(&mut spec, "mode", Json::Str(owned("retire-rollback")?))?;
    }
    if opts.recover {
        set(&mut spec, "mode", Json::Str(owned("recover")?))?;
    }
    if opts.rollback {
        set(&mut spec, "mode", Json::Str(owned("rollback")?))?;
    }
    if opts.restore {
        set(
            &mut spec,
            "mode",
            Json::Str(owned("restore-shared-services")?),
        )?;
    }
    if opts.refresh {
        set(
            &mut spec,
            "mode",
            Json::Str(owned("refresh-notifications")?),
        )?;
    }
    set(&mut spec, "irreversible", Json::Bool(opts.irreversible))?;
    if let Some(target) = opts.schema_target {
        set(&mut spec, "schema_target", Json::from(target))?;
    }
    if let Some(revision) = opts.revision {
        set(
            &mut spec,
            "authorised_revision",
            Json::Str(owned(revision)?),
        )?;
    }
    if let Some((source, live_minor)) = &live_server {
        // What the launcher read; the controller requires the same.
        set(
            &mut spec,
            "postgres_launcher_minor",
            Json::Str(fmt(format_args!("17.{live_minor}"))?),
        )?;
        set(
            &mut spec,
            "postgres_launcher_source",
            Json::Str(owned(source)?),
        )?;
    }
    if let Some(minor) = debian_minor {
        set(&mut spec, "mode", Json::Str(owned("postgres-to-debian")?))?;
        set(
            &mut spec,
            "postgres_target_minor",
            Json::Str(fmt(format_args!("17.{minor}"))?),
        )?;
    }
    if let Some(hash) = policy_sha256 {
        set(&mut spec, "mode", Json::Str(owned("converge-config")?))?;
        set(&mut spec, "policy_sha256", Json::Str(hash))?;
        set(&mut spec, "plan_only", Json::Bool(opts.plan))?;
        let expect = match opts.expect {
            Some(digest) => Json::Str(owned(digest)?),
            None => Json::Null,
        };
        set(&mut spec, "expect_digest", expect)?;
    }
    if let Some(built) = &closure {
        set(&mut spec, "mode", Json::Str(owned("postgres-binaries")?))?;
        let mut entries = Vec::new();
        for item in built {
            let mut entry = Json::Obj(Vec::new());
            set(&mut entry, "basename", Json::Str(owned(&item.basename)?))?;
            set(&mut entry, "nar_sha256", Json::Str(owned(&item.sha256)?))?;
            td_alloc::push(&mut entries, entry)?;
        }
        set(&mut spec, "closure", Json::Arr(entries))?;
    }
    set(&mut spec, "budget", Json::from(opts.budget))?;
    set(&mut spec, "verdict", verdict.map_or(Json::Null, Json::Str))?;
    require(
        !stepping || fmt(format_args!("{spec}"))?.len() <= 4096,
        "the stepping-stone job exceeds what the loopback-TLS controller reads",
    )?;
    local.save(&fmt(format_args!("{job}/job.json"))?, &spec)?;
    let receipt = fmt(format_args!("{job}/receipt.json"))?;
    println!("Production receipt: {receipt}");
    let probe_path = fmt(format_args!("{job}/public-jmap.json"))?;
    let mut probe = Json::Obj(Vec::new());
    for (k, v) in [
        ("check", "authenticated public JMAP Mailbox/get"),
        ("revision", selected.as_str()),
        ("job", "pending"),
        ("after_result", "pending"),
    ] {
        set(&mut probe, k, Json::Str(owned(v)?))?;
    }
    set(&mut probe, "before", before.map_or(Json::Null, Json::from))?;
    local.save(&probe_path, &probe)?;
    let unit = fmt(format_args!("--unit=td-mta-deploy-{token}"))?;
    if opts.converge {
        state.launched = Some(converge::Launch {
            receipt: owned(&receipt)?,
            unit: fmt(format_args!("td-mta-deploy-{token}"))?,
        });
    }
    let working = fmt(format_args!("--property=WorkingDirectory={job}"))?;
    let status = local
        .ssh(&[
            "sudo",
            "-n",
            "systemd-run",
            "--wait",
            "--collect",
            "--quiet",
            &unit,
            "--property=UMask=0077",
            "--property=TimeoutStopSec=360",
            "--property=KillMode=mixed",
            &working,
            &program,
            if opts.socket {
                "--postgres-socket"
            } else if opts.to_debian {
                "--postgres-to-debian"
            } else if opts.binaries {
                "--postgres-binaries"
            } else if opts.retire {
                "--retire-rollback"
            } else if opts.recover {
                "--recover"
            } else if opts.rollback {
                "--rollback"
            } else if opts.converge {
                "--converge-config"
            } else if opts.restore {
                "--restore-shared-services"
            } else if opts.refresh {
                "--refresh-notifications"
            } else {
                "--activate"
            },
        ])?
        .stdin(Stdio::null())
        .status()?;
    if opts.converge {
        set(
            &mut probe,
            "job",
            Json::Str(owned(if status.code() == Some(255) {
                "unknown"
            } else if status.success() {
                "succeeded"
            } else {
                "failed"
            })?),
        )?;
        let post = local.qualify_jmap(account, &target.jmap_url);
        set(&mut probe, "after", Json::from(now()?))?;
        set(
            &mut probe,
            "after_result",
            Json::Str(owned(if post.is_ok() { "passed" } else { "failed" })?),
        )?;
        local.save(&probe_path, &probe)?;
        let printed = local.sudo(&["cat", &receipt]);
        if let Ok(printed) = &printed {
            println!("{printed}");
        }
        // Absent, as opposed to unreadable, when `test -e` says so.
        let missing = printed.is_err()
            && local
                .ssh(&["sudo", "-n", "test", "-e", &receipt])?
                .stdin(Stdio::null())
                .status()?
                .code()
                == Some(1);
        // A job refused before its receipt records why beside it.
        let refusal = if missing {
            fmt(format_args!("{job}/refusal.json"))
                .ok()
                .and_then(|path| local.sudo(&["cat", &path]).ok())
                .and_then(|text| td_json::parse(&text).ok())
                .and_then(|record| record.get("error").and_then(Json::as_str).map(owned))
                .transpose()?
        } else {
            None
        };
        let receipt = match (&printed, &refusal) {
            (Ok(text), _) => converge::Receipt::Text(text),
            (Err(_), Some(error)) => converge::Receipt::Refused(error),
            (Err(_), None) if missing => converge::Receipt::Missing,
            (Err(_), None) => converge::Receipt::Unreadable,
        };
        // A signal or a lost connection: the detached job may still run.
        let job = if matches!(status.code(), Some(255) | None) {
            converge::Job::Lost
        } else if status.success() {
            converge::Job::Succeeded
        } else {
            converge::Job::Failed
        };
        return converge::finish(
            &mut std::io::stdout().lock(),
            state,
            receipt,
            job,
            post,
            &converge::carried(args)?,
        )
        .map(|()| None);
    }
    // The job ran: nothing from here may end the command without its
    // outcome, so what cannot be read is reported as not known.
    let exit = status.code();
    let post = if opts.refresh {
        // Touching no service, a notification refresh makes no check after.
        None
    } else {
        Some(local.qualify_jmap(account, &target.jmap_url).is_ok())
    };
    let saved = (|| -> R<()> {
        let job = match exit {
            Some(255) | None => "unknown",
            Some(0) => "succeeded",
            Some(_) => "failed",
        };
        set(&mut probe, "job", Json::Str(owned(job)?))?;
        if let Some(post) = post {
            set(&mut probe, "after", Json::from(now()?))?;
            let after = if post { "passed" } else { "failed" };
            set(&mut probe, "after_result", Json::Str(owned(after)?))?;
        }
        local.save(&probe_path, &probe)
    })();
    if let Err(e) = saved {
        eprintln!("td-mta-deploy: the public check record was not saved: {e}");
    }
    let read = |path: &str| {
        local
            .sudo(&["cat", path])
            .ok()
            .and_then(|text| td_json::parse(&text).ok())
    };
    let mut record = read(&receipt);
    let mut receipt = receipt;
    if record.is_none() {
        if let Ok(refusal) = fmt(format_args!("{job}/refusal.json")) {
            record = read(&refusal);
            if record.is_some() {
                receipt = refusal;
            }
        }
    }
    if let Some(record) = &record {
        println!("{record}");
    }
    let fence = match local.remote_status(&["sudo", "-n", "test", "-e", FENCE]) {
        Ok(Some(0)) => Some(true),
        Ok(Some(1)) => Some(false),
        _ => None,
    };
    Ok(Some(Job {
        receipt,
        record,
        exit,
        fence,
        post,
        stepping,
    }))
}

/// How a job ended, as the launcher saw it.
struct Job {
    receipt: String,
    record: Option<Json>,
    /// The remote command's exit code; ssh's 255 is its own failure.
    exit: Option<i32>,
    /// Whether the deployment fence was held afterwards, if known.
    fence: Option<bool>,
    /// The public JMAP check after the job, when one was made.
    post: Option<bool>,
    /// The job deployed a stepping stone under the loopback-TLS
    /// controller, which --recover does not model.
    stepping: bool,
}

/// A refusal before a job and the command that follows it, if any;
/// `run` writes its outcome line.
#[derive(Debug)]
pub(super) struct Refusal {
    pub(super) error: String,
    pub(super) next: Option<Vec<String>>,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)
    }
}

impl std::error::Error for Refusal {}

/// A job's outcome: what happened and the command that follows.
struct Ended {
    result: &'static str,
    error: Option<String>,
    next: Option<Vec<String>>,
    /// A command that takes a decision `next` never takes for an agent,
    /// and the decisions it takes.
    approve: Option<(Vec<String>, Vec<&'static str>)>,
    not_before: Option<u64>,
}

/// The flags that each take a decision an agent takes only on purpose,
/// and the name `needs` gives that decision.
const DECISIONS: &[(&str, &str)] = &[
    ("--irreversible", "irreversible-approval"),
    ("--skip-tests", "skip-tests-approval"),
];

/// `end` with a `next` that takes a decision moved to `approve`: what
/// follows is never a decision an agent takes by following it.
fn held(mut end: Ended) -> Ended {
    let Some(argv) = end.next.take() else {
        return end;
    };
    let mut needs = Vec::new();
    for (flag, decision) in DECISIONS {
        if argv.iter().any(|word| word == flag) && td_alloc::push(&mut needs, *decision).is_err() {
            // Without room to name the decision, offer no command at all.
            return end;
        }
    }
    if needs.is_empty() {
        end.next = Some(argv);
    } else {
        end.approve = Some((argv, needs));
    }
    end
}

/// `{"argv": [...], "needs": [...]}`.
fn command_json(argv: &[String], needs: &[&str]) -> R<Json> {
    let mut words = Vec::new();
    for word in argv {
        td_alloc::push(&mut words, word.as_str())?;
    }
    let mut object = Json::Obj(Vec::new());
    set(&mut object, "argv", strings(&words)?)?;
    set(&mut object, "needs", strings(needs)?)?;
    Ok(object)
}

/// `td-mta/deploy`, `flags`, and the options this run was given that
/// every mode shares.
fn command(opts: &Options<'_>, flags: &[&str]) -> R<Vec<String>> {
    let mut argv = Vec::new();
    td_alloc::push(&mut argv, owned("td-mta/deploy")?)?;
    for flag in flags {
        td_alloc::push(&mut argv, owned(flag)?)?;
    }
    if let Some(config) = opts.config {
        td_alloc::push(&mut argv, owned("--config")?)?;
        td_alloc::push(&mut argv, owned(config)?)?;
    }
    if let Some(account) = opts.account {
        td_alloc::push(&mut argv, owned("--jmap-account")?)?;
        td_alloc::push(&mut argv, owned(account)?)?;
    }
    Ok(argv)
}

/// Whether `opts` deploys a release: no other mode is named.
fn activation(opts: &Options<'_>) -> bool {
    !(opts.socket
        || opts.binaries
        || opts.retire
        || opts.recover
        || opts.rollback
        || opts.restore
        || opts.refresh
        || opts.converge)
}

/// This run's own command again, with `extra` after its arguments.
fn again(args: &[String], extra: &[&str]) -> R<Vec<String>> {
    let mut argv = Vec::new();
    td_alloc::push(&mut argv, owned("td-mta/deploy")?)?;
    for arg in args {
        td_alloc::push(&mut argv, owned(arg)?)?;
    }
    for arg in extra {
        td_alloc::push(&mut argv, owned(arg)?)?;
    }
    Ok(argv)
}

/// The end of a run refused before its job, naming `next`: `busy`, and
/// this run again, while another job runs on a loopback-TLS host.
fn refused_end(error: String, next: Option<Vec<String>>, args: &[String]) -> R<Ended> {
    let busy = error == transport::RUNNING;
    Ok(held(Ended {
        result: if busy { "busy" } else { "refused" },
        next: if busy { Some(again(args, &[])?) } else { next },
        error: Some(error),
        approve: None,
        not_before: None,
    }))
}

/// What follows a refusal before any job: after main moved past an
/// authorised revision, the run without the authorisation, which finds
/// what the new one migrates; otherwise nothing.
fn refused_next(message: &str, args: &[String]) -> R<Option<Vec<String>>> {
    if message == MOVED {
        Ok(Some(again(&unauthorised(args)?, &[])?))
    } else {
        Ok(None)
    }
}

/// The schema version and revision the controller's migration refusal
/// names, in the order `activate::migrates` writes them.
fn migration(error: &str) -> Option<(&str, &str)> {
    let (_, flags) = error.rsplit_once("run td-mta/deploy --irreversible --schema-target ")?;
    let (target, revision) = flags.split_once(" --revision ")?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let hex = revision.len() == 40
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (digits(target) && hex).then_some((target, revision))
}

/// This run's arguments without the migration it authorised.
fn unauthorised(args: &[String]) -> R<Vec<String>> {
    let mut kept = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--irreversible" => {}
            "--schema-target" | "--revision" => {
                it.next();
            }
            // Values of the options that take one are kept as given.
            "--config" | "--jmap-account" | "--window-seconds" => {
                td_alloc::push(&mut kept, owned(arg)?)?;
                if let Some(value) = it.next() {
                    td_alloc::push(&mut kept, owned(value)?)?;
                }
            }
            _ => td_alloc::push(&mut kept, owned(arg)?)?,
        }
    }
    Ok(kept)
}

/// The flags of the command a refusal names in its `run td-mta/deploy
/// --FLAG [VALUE]...`, up to the first word that is neither.
fn named(error: &str) -> Option<Vec<&str>> {
    let (_, rest) = error.split_once("run td-mta/deploy")?;
    // Bare, it names plain td-mta/deploy, which names the step itself.
    if rest.is_empty() || rest.starts_with([',', '.', ';', ':']) {
        return Some(Vec::new());
    }
    let rest = rest.strip_prefix(' ')?;
    let mut flags = Vec::new();
    for word in rest.split_ascii_whitespace() {
        let value = word.bytes().all(|b| b.is_ascii_digit());
        if !(word.starts_with("--") || (value && !flags.is_empty())) {
            break;
        }
        td_alloc::push(&mut flags, word).ok()?;
    }
    // An irreversible step is never a follow-up a message can name.
    (!flags.is_empty() && !flags.contains(&"--irreversible")).then_some(flags)
}

/// What `job`'s end says: its receipt's error and fields, the fence, the
/// exit code and the public check after it. `args` repeats this run.
fn ended(opts: &Options<'_>, args: &[String], job: &Job) -> R<Ended> {
    let text = |key| {
        job.record
            .as_ref()
            .and_then(|r| r.get(key))
            .and_then(Json::as_str)
    };
    let not_before = job
        .record
        .as_ref()
        .and_then(|r| r.get("rollback_retire_not_before"))
        .and_then(Json::as_u64);
    let error = match text("error") {
        Some(e) => Some(owned(e)?),
        None => None,
    };
    let recover = || command(opts, &["--recover"]);
    let unknown = |why: &str| -> R<Ended> {
        Ok(Ended {
            result: "unknown",
            error: Some(owned(why)?),
            next: Some(recover()?),
            approve: None,
            not_before: None,
        })
    };
    // A fence recovery refuses by its kind is not one a rerun finishes.
    let unrecoverable = error
        .as_deref()
        .is_some_and(|e| crate::activate::UNRECOVERABLE.contains(&e));
    let names_recover = error
        .as_deref()
        .and_then(named)
        .is_some_and(|flags| flags == ["--recover"]);
    // What a finished job leaves to do: the switch it kept the Debian
    // tree for, or the retirement still to come.
    let finished = || -> R<Option<Vec<String>>> {
        Ok(if text("debian_tree_kept").is_some() {
            Some(command(opts, &["--postgres-binaries"])?)
        } else if not_before.is_some() {
            Some(again(args, &[])?)
        } else {
            None
        })
    };
    let lost = matches!(job.exit, None | Some(255));
    let end = match (job.exit, job.fence) {
        // The stepping stone ran under the loopback-TLS controller, whose
        // jobs and fence --recover does not model.
        (exit, fence) if job.stepping && exit != Some(0) && (lost || fence != Some(false)) => {
            Ended {
                result: if lost { "unknown" } else { "failed" },
                error: Some(owned(STEPPING_LEFT)?),
                next: None,
                approve: None,
                not_before: None,
            }
        }
        (Some(0), _) if job.stepping && job.post == Some(false) => Ended {
            result: "check-failed",
            error: Some(owned(
                "the stepping stone is deployed but the public JMAP check after it failed",
            )?),
            next: None,
            approve: None,
            not_before: None,
        },
        (Some(0), _) if job.stepping => Ended {
            result: "stepping-stone-deployed",
            error: None,
            next: Some(command(opts, &["--postgres-socket"])?),
            approve: None,
            not_before: None,
        },
        (None | Some(255), _) => unknown(
            "the SSH connection ended before the job reported: --recover finishes it once it stops",
        )?,
        // Whether to roll back is the operator's decision, not a step.
        (Some(0), _) if job.post == Some(false) && activation(opts) => Ended {
            result: "check-failed",
            error: Some(owned(
                "the deployment finished but the public JMAP check after it failed: if the failure persists, td-mta/deploy --rollback returns to the release before the live one",
            )?),
            next: None,
            approve: None,
            not_before: None,
        },
        (Some(0), _) if job.post == Some(false) => Ended {
            result: "check-failed",
            error: Some(owned(
                "the job finished but the public JMAP check after it failed",
            )?),
            next: finished()?,
            approve: None,
            not_before,
        },
        (Some(0), _) => Ended {
            result: "succeeded",
            error: None,
            next: finished()?,
            approve: None,
            not_before,
        },
        // Another job holds the host: this one changed nothing.
        (Some(_), _) if error.as_deref() == Some(crate::LOCKED) => Ended {
            result: "busy",
            error,
            next: Some(again(args, &[])?),
            approve: None,
            not_before: None,
        },
        // --recover does not model the socket cutover's fence; a fence
        // the cutover found held, and names --recover for, it does.
        (Some(_), Some(true)) if opts.socket && !names_recover => Ended {
            result: "failed",
            error: Some(fmt(format_args!(
                "{}; {SOCKET_LEFT}",
                error.as_deref().unwrap_or("the job failed")
            ))?),
            next: None,
            approve: None,
            not_before: None,
        },
        (Some(_), Some(true)) => Ended {
            result: "failed",
            error: Some(error.map_or_else(|| owned("the job failed"), Ok)?),
            next: if unrecoverable {
                None
            } else {
                Some(recover()?)
            },
            approve: None,
            not_before: None,
        },
        (Some(_), None) => unknown(
            "the job failed and whether it left the deployment fence could not be read: --recover finishes it if it did",
        )?,
        (Some(_), _) => {
            let (result, next) = match error.as_deref() {
                Some(e) if e.starts_with(crate::activate::MIGRATES) => (
                    "refused-irreversible",
                    match migration(e) {
                        Some((target, revision)) => Some(again(
                            &unauthorised(args)?,
                            &[
                                "--irreversible",
                                "--schema-target",
                                target,
                                "--revision",
                                revision,
                            ],
                        )?),
                        None => None,
                    },
                ),
                Some(crate::activate::INSIDE_WINDOW) => ("deferred", Some(again(args, &[])?)),
                Some(e) => match named(e) {
                    Some(flags) if !(unrecoverable || opts.recover && flags == ["--recover"]) => {
                        ("failed", Some(command(opts, &flags)?))
                    }
                    _ => ("failed", None),
                },
                None => ("failed", None),
            };
            Ended {
                result,
                error: Some(error.map_or_else(
                    || owned("the job failed before it wrote a record: see the host's journal for its unit"),
                    Ok,
                )?),
                next,
                approve: None,
                not_before: if result == "deferred" {
                    not_before
                } else {
                    None
                },
            }
        }
    };
    Ok(held(end))
}

#[cfg(test)]
#[path = "tests/launcher.rs"]
mod tests;
