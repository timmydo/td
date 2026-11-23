//! Select a tested main revision and submit a pinned, standalone operator job.
use crate::{
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
#[path = "launcher_files.rs"]
mod files;

const RELEASES: &str = "/opt/td-mta/lib/td-mta/releases/git-";
const OPERATORS: &str = "/opt/td-mta/lib/td-mta/deploy";
const HELP: &str = "Usage: td-mta/deploy [--skip-tests] [OPTIONS]
       td-mta/deploy --postgres-socket [OPTIONS]
       td-mta/deploy --postgres-binaries [OPTIONS]
       td-mta/deploy --write-target --host HOST --host-name NAME --account ACCOUNT
         --identity KEY --jmap-account ACCOUNT --jmap-url URL [--config FILE] [--replace]
       td-mta/deploy --provision-host --build-account ACCOUNT --build-home DIR
         --public-name NAME [--config FILE] [--replace]
Options: [--window-seconds 1..3600] [--jmap-account ACCOUNT] [--config FILE]
Select a verified origin/main revision and deploy it to the configured host,
move the live release's database from loopback TLS to its private socket, or
switch it from the Debian PostgreSQL to td's source-built one, applying
what the reviewed minor-release table says each release in between needs.
The target is FILE, else $XDG_CONFIG_HOME/td-mta-deploy/target.json, else
~/.config/td-mta-deploy/target.json (td-mta/PRODUCTION.md). --write-target
writes that file; --provision-host then prepares the host's build account
and installs its /var/lib/td-mta-operator/host.json. Either refuses to
change an existing file that says something else unless --replace.";
struct Options<'a> {
    skip: bool,
    budget: u64,
    account: Option<&'a str>,
    config: Option<&'a str>,
    socket: bool,
    binaries: bool,
}
fn parse(args: &[String]) -> R<Options<'_>> {
    let mut out = Options {
        skip: false,
        budget: 60,
        account: None,
        config: None,
        socket: false,
        binaries: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--skip-tests" => out.skip = true,
            "--window-seconds" => {
                out.budget = it.next().ok_or("missing window")?.parse()?;
                require(
                    (1..=3600).contains(&out.budget),
                    "window must be 1..3600 seconds",
                )?;
            }
            "--jmap-account" => out.account = Some(it.next().ok_or("missing JMAP account")?),
            "--config" => out.config = Some(it.next().ok_or("missing target configuration")?),
            "--postgres-socket" => out.socket = true,
            "--postgres-binaries" => out.binaries = true,
            _ => return Err(HELP.into()),
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
    Ok(out)
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
            "-i",
        ])
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
                    return Ok((owned(rev)?, Some(row)));
                }
            }
        }
        if !revs.is_empty() {
            println!("No newer landed revision has a passing main-integration verdict. Checking the deployed release.");
        }
        Ok((owned(live)?, None))
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
    // Early and read-only, before a build of up to hours: the live Debian
    // release, which the minor-release table must list.
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
    let local = Local {
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
    error: Option<&'a str>,
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
    if let Some(error) = outcome.error {
        set(&mut line, "error", Json::Str(owned(error)?))?;
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
    let mut out = std::io::stdout().lock();
    std::io::Write::write_all(&mut out, outcome_line(outcome)?.as_bytes())?;
    std::io::Write::flush(&mut out)?;
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
    let opts = parse(args)?;
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
    let (selected, verdict) = if opts.socket || opts.binaries {
        require(
            local.is_ancestor(live, &tip)?,
            "live release is not an ancestor of advertised main",
        )?;
        (owned(live)?, None)
    } else {
        local.select(&tip, live, opts.skip, &child(&base, "verdicts")?)?
    };
    let tree = local.host.out(
        local
            .git()
            .args(["rev-parse", &fmt(format_args!("{selected}^{{tree}}"))?]),
    )?;
    revision(&tree)?;
    println!("{} current {live}, selected {selected}", site.host_name);
    let live_minor = if opts.binaries {
        Some(local.debian_release()?)
    } else {
        None
    };
    let cache = child(&local.root, ".td-build-cache")?;
    fs::create_dir_all(&cache)?;
    let scratch = td_fs::TempDir::new_in(&cache, "mta-deploy")?;
    let binary = local.artifact(scratch.path(), &controller)?;
    let closure = match live_minor {
        Some(live_minor) => {
            let built = local.closure(scratch.path())?;
            let server = built.first().ok_or("missing PostgreSQL output")?;
            let td_minor = postgres_minor(store_basename(&server.basename, "postgresql-x86-64")?)?;
            // Before anything reaches the host: the table covers the move.
            crate::postgres_minors::movable(live_minor, td_minor)?;
            Some((built, live_minor))
        }
        None => None,
    };
    let before = local.qualify_jmap(account, &target.jmap_url)?;
    if selected != live {
        let key = local.key.to_str().ok_or("invalid SSH key path")?;
        let mirror = site.mirror()?;
        let remote = fmt(format_args!(
            "{}:{}",
            local.destination,
            mirror.to_str().ok_or("invalid source mirror")?
        ))?;
        let ssh = words(&[
            "ssh",
            "-T",
            "-oConnectTimeout=10",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oIdentitiesOnly=yes",
            "-i",
            key,
        ])?;
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
    for (built, _) in closure.iter() {
        for built in built {
            local.send_nar(built, &job)?;
        }
    }
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
    if let Some((built, live_minor)) = &closure {
        set(&mut spec, "mode", Json::Str(owned("postgres-binaries")?))?;
        let mut entries = Vec::new();
        for item in built {
            let mut entry = Json::Obj(Vec::new());
            set(&mut entry, "basename", Json::Str(owned(&item.basename)?))?;
            set(&mut entry, "nar_sha256", Json::Str(owned(&item.sha256)?))?;
            td_alloc::push(&mut entries, entry)?;
        }
        set(&mut spec, "closure", Json::Arr(entries))?;
        // What the launcher read; the controller requires the same.
        set(
            &mut spec,
            "postgres_launcher_minor",
            Json::Str(fmt(format_args!("17.{live_minor}"))?),
        )?;
    }
    set(&mut spec, "budget", Json::from(opts.budget))?;
    set(&mut spec, "verdict", verdict.map_or(Json::Null, Json::Str))?;
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
    set(&mut probe, "before", Json::from(before))?;
    local.save(&probe_path, &probe)?;
    let unit = fmt(format_args!("--unit=td-mta-deploy-{token}"))?;
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
            } else if opts.binaries {
                "--postgres-binaries"
            } else {
                "--activate"
            },
        ])?
        .stdin(Stdio::null())
        .status()?;
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
    println!("{}", local.sudo(&["cat", &receipt])?);
    require(
        status.success(),
        "remote job failed or disconnected; inspect the job receipt",
    )?;
    post?;
    Ok(())
}
#[cfg(test)]
#[path = "tests/launcher.rs"]
mod tests;
