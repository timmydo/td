//! Select a tested main revision and submit a pinned, standalone operator job.
use crate::{
    common::{child, digest, fmt, owned, require, set, Host},
    probes::now,
    R,
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use td_json::Json;

const REMOTE: &str = "timmy@172.179.0.6";
const RELEASES: &str = "/opt/td-mta/lib/td-mta/releases/git-";
const OPERATORS: &str = "/opt/td-mta/lib/td-mta/deploy";
const HELP:&str="Usage: td-mta/deploy [--skip-tests] [--window-seconds 1..3600] [--jmap-account ACCOUNT]\nSelect a verified origin/main revision and deploy it to claw1.";
struct Options<'a> {
    skip: bool,
    budget: u64,
    account: &'a str,
}
fn parse(args: &[String]) -> R<Options<'_>> {
    let mut out = Options {
        skip: false,
        budget: 60,
        account: "xyz882015",
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
            "--jmap-account" => out.account = it.next().ok_or("missing JMAP account")?,
            _ => return Err(HELP.into()),
        }
    }
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
        .arg(REMOTE)
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
        let tmp = fmt(format_args!("{path}.pending"))?;
        self.host.exec(
            &mut self.ssh(&["sudo", "-n", "install", "-m", "0600", "/dev/stdin", &tmp])?,
            fmt(format_args!("{value}"))?.as_bytes(),
            Duration::from_secs(30),
            true,
        )?;
        self.sudo(&["mv", "-T", &tmp, path])?;
        self.sudo(&["sync", "-f", path])?;
        Ok(())
    }
    fn upload(&self, file: &Path, dst: &str) -> R<()> {
        let mut c = self.ssh(&["sudo", "-n", "install", "-m", "0755", "/dev/stdin", dst])?;
        let out = c.stdin(File::open(file)?).stdout(Stdio::null()).status()?;
        require(out.success(), "controller upload failed")
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
        let mut build = Command::new("cargo");
        build.current_dir(&checkout)
            .args(["build","--frozen","--release","--target","x86_64-unknown-linux-musl","--bin","td-mta-deploy","--manifest-path","td-mta/Cargo.toml","--target-dir"]).arg(&target)
            .env("CARGO_ENCODED_RUSTFLAGS",fmt(format_args!("-Clinker=rust-lld\x1f-Ctarget-feature=+crt-static\x1f--remap-path-prefix={}=/td-deploy-source",checkout.display()))?);
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
        self.upload(bin, &stage)?;
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
    fn qualify_jmap(&self, account: &str) -> R<f64> {
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
                    && matches!(
                        a.get("well_known_url").and_then(Json::as_str),
                        Some(
                            "https://mx.timmydouglas.com/.well-known/jmap"
                                | "https://mx.timmyd.info/.well-known/jmap"
                                | "https://mx.882015.xyz/.well-known/jmap"
                        )
                    )
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
pub(super) fn run(args: &[String]) -> R<()> {
    if matches!(args,[s] if s=="--help"||s=="-h") {
        println!("{HELP}");
        return Ok(());
    }
    let opts = parse(args)?;
    let host = Host::default();
    let root = PathBuf::from(host.out(Command::new("git").args(["rev-parse", "--show-toplevel"]))?);
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("operator home")?);
    let local = Local {
        host,
        root,
        key: child(&home, ".ssh/td_mta_systemd_test")?,
    };
    let (tip, controller) = local.origin_revisions()?;
    let live = local.remote(&["readlink", "-f", "/opt/td-mta/bin/td-mta"])?;
    let live = live_path(&live)?;
    let base = match std::env::var_os("TD_MAIN_INTEGRATION_DIR") {
        Some(p) => PathBuf::from(p),
        None => child(&home, ".local/state/td/main-integration")?,
    };
    let (selected, verdict) = local.select(&tip, live, opts.skip, &child(&base, "verdicts")?)?;
    let tree = local.host.out(
        local
            .git()
            .args(["rev-parse", &fmt(format_args!("{selected}^{{tree}}"))?]),
    )?;
    revision(&tree)?;
    println!("claw1 current {live}, selected {selected}");
    let cache = child(&local.root, ".td-build-cache")?;
    fs::create_dir_all(&cache)?;
    let scratch = td_fs::TempDir::new_in(&cache, "mta-deploy")?;
    let binary = local.artifact(scratch.path(), &controller)?;
    let before = local.qualify_jmap(opts.account)?;
    if selected != live {
        let key = local.key.to_str().ok_or("invalid SSH key path")?;
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
                "timmy@172.179.0.6:/home/timmy/src/td",
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
            "--activate",
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
    let post = local.qualify_jmap(opts.account);
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
