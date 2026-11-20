use super::*;
use std::{
    io::{Read, Seek, SeekFrom},
    os::unix::process::CommandExt,
    path::Component,
    process::Stdio,
};

const BUILD_USER: &str = "timmy";
const BUILD_ROOT: &str = "/home/timmy/.local/state/td-mta-candidates";
const MIRROR: &str = "/home/timmy/src/td";
const COPY_LIMIT: u64 = 512 * 1024 * 1024;
const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

impl Deployment {
    fn build_command(&self) -> Command {
        let mut cmd = self.host.cmd("/usr/sbin/runuser");
        cmd.env_clear().env("PATH", SAFE_PATH).env("LANG", "C").env("LC_ALL", "C")
            .args(["-u", BUILD_USER, "--", "/usr/bin/env", "HOME=/home/timmy",
                "PATH=/home/timmy/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]);
        cmd
    }

    pub(super) fn build(&mut self) -> R<()> {
        let revision = field(&self.record, "revision")?;
        let tree = field(&self.record, "tree")?;
        let dest = parent(&self.candidate)?;
        if dest.try_exists()? {
            let stamp = read_json(&child(dest, "deployment-inputs.json")?, 1024 * 1024)?;
            let hash = digest(&self.candidate)?;
            require(
                field(&stamp, "revision")? == revision
                    && field(&stamp, "tree")? == tree
                    && field(&stamp, "binary_sha256")? == hash,
                "retained release provenance",
            )?;
            return set(&mut self.record, "binary_sha256", Json::Str(hash));
        }
        let prefix = PathBuf::from(
            self.host.out(
                self.build_command()
                    .args(["mktemp", "-d"])
                    .arg(child(Path::new(BUILD_ROOT), "deploy-XXXXXXXX")?),
            )?,
        );
        require(
            prefix.parent() == Some(Path::new(BUILD_ROOT))
                && numbered_name(name(&prefix)?, "deploy-"),
            "unrecognized build directory",
        )?;
        let source = child(&prefix, "src")?;
        let checkout = self.host.run(
            self.build_command()
                .args(["git", "-C", MIRROR, "worktree", "add", "--detach"])
                .arg(&source)
                .arg(revision),
        );
        let mut clean = Ok(());
        let result = if checkout.is_ok() {
            let result = self.build_in(&source, &prefix, tree, revision);
            // Release the worktree as its owner even when a build is interrupted.
            let cleanup = Host::default();
            cleanup.recover();
            clean = cleanup
                .exec(
                    self.build_command()
                        .args(["git", "-C", MIRROR, "worktree", "remove", "--force"])
                        .arg(&source),
                    &[],
                    Duration::from_secs(120),
                    true,
                )
                .map(|_| ());
            result
        } else {
            checkout.and(Err("checkout failed".into()))
        };
        let cleanup = Host::default();
        cleanup.recover();
        let remove = cleanup.exec(
            self.build_command().args(["rm", "-rf", "--"]).arg(&prefix),
            &[],
            Duration::from_secs(120),
            true,
        );
        let hash = result?;
        clean?;
        remove?;
        set(&mut self.record, "binary_sha256", Json::Str(hash))
    }

    fn build_in(&self, source: &Path, prefix: &Path, tree: &str, revision: &str) -> R<String> {
        require(
            self.host.out(
                self.build_command()
                    .args(["git", "-C"])
                    .arg(source)
                    .args(["rev-parse", "HEAD^{tree}"]),
            )? == tree,
            "remote checkout tree mismatch",
        )?;
        let log = create_file(&child(&self.work, "build.log")?, 0o600)?;
        let installed = child(prefix, "installed")?;
        self.host.run_to(
            self.build_command()
                .arg(child(source, "install-td-mta")?)
                .arg("--prefix")
                .arg(&installed)
                .current_dir(source),
            Duration::from_secs(3600),
            &log,
            Some(&log),
        )?;
        let binary = PathBuf::from(
            self.host.out(
                self.build_command()
                    .args(["readlink", "-e"])
                    .arg(child(&installed, "bin/td-mta")?),
            )?,
        );
        require(
            binary.starts_with(child(&installed, "lib/td-mta/releases")?)
                && name(&binary)? == "td-mta",
            "build release path",
        )?;
        let sum = self
            .host
            .out(self.build_command().args(["sha256sum", "--"]).arg(&binary))?;
        let expected = sum
            .split_ascii_whitespace()
            .next()
            .ok_or("missing build hash")?;
        require(lower_hex(expected, 64), "invalid build hash")?;
        let stage = td_fs::TempDir::new_in(Path::new(RELEASES), "stage")?;
        self.import_files(parent(&binary)?, stage.path())?;
        require(
            child(stage.path(), "BUILD-INPUTS")?.is_file()
                && child(stage.path(), "notices")?.is_dir()
                && digest(&child(stage.path(), "td-mta")?)? == expected,
            "release inputs or hash",
        )?;
        let mut stamp = Json::Obj(Vec::new());
        for (key, value) in [
            ("revision", revision),
            ("tree", tree),
            ("binary_sha256", expected),
        ] {
            set(&mut stamp, key, Json::Str(owned(value)?))?;
        }
        durable(&child(stage.path(), "deployment-inputs.json")?, &stamp)?;
        fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o755))?;
        fs::rename(stage.path(), parent(&self.candidate)?)?;
        self.host
            .run(self.host.cmd("sync").arg("-f").arg(Path::new(RELEASES)))?;
        owned(expected)
    }

    // The build account enumerates and reads its own tree. Only regular files
    // and directories enter root's private stage. Never give an unprivileged
    // process a writable descriptor to an inode that will become executable.
    fn import_files(&self, source: &Path, stage: &Path) -> R<()> {
        let limit = Instant::now() + Duration::from_secs(120);
        let manifest = self.host.exec(
            self.build_command()
                .args(["find", ".", "-printf", "%y%P\\0"])
                .current_dir(source),
            &[],
            Duration::from_secs(40),
            true,
        )?;
        let text = std::str::from_utf8(&manifest.stdout)?;
        let mut data = text
            .strip_suffix('\0')
            .ok_or("incomplete release inventory")?
            .split('\0');
        require(data.next() == Some("d"), "release inventory root")?;
        let mut remaining = COPY_LIMIT;
        let scratch = td_fs::TempDir::new_in(&self.work, "import")?;
        let temp = child(scratch.path(), "bytes")?;
        for entry in data {
            self.host.remaining()?;
            require(Instant::now() < limit, "release import timeout")?;
            let (kind, relative) = release_member(entry)?;
            let target = child(stage, relative)?;
            if kind == b'd' {
                fs::create_dir(&target)?;
                fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
                continue;
            }
            let mut input = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            fs::remove_file(&temp)?;
            let budget = limit
                .checked_duration_since(Instant::now())
                .ok_or("release import timeout")?;
            self.host.run_to(
                self.build_command()
                    .arg("prlimit")
                    .arg(fmt(format_args!("--fsize={remaining}"))?)
                    .args(["--", "dd", "status=none", "iflag=nofollow,nonblock"])
                    .arg(fmt(format_args!("if={relative}"))?)
                    .current_dir(source),
                budget,
                &input,
                None,
            )?;
            input.seek(SeekFrom::Start(0))?;
            let mut output = create_file(&target, 0o600)?;
            let size = io::copy(&mut input.take(remaining + 1), &mut output)?;
            remaining = remaining.checked_sub(size).ok_or("release size limit")?;
            output.set_permissions(fs::Permissions::from_mode(if relative == "td-mta" {
                0o755
            } else {
                0o644
            }))?;
        }
        Ok(())
    }

    pub(super) fn copy_cold(&self, warm: bool) -> R<()> {
        require(
            fs::metadata(PGDATA)?.dev() == fs::metadata(&self.work)?.dev(),
            "primary rollback device",
        )?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.cold)?;
        crate::physical::check_no_links(Path::new(PGDATA))?;
        let mounts = self.host.out(self.host.cmd("findmnt").args([
            "--noheadings",
            "--raw",
            "--output",
            "TARGET",
        ]))?;
        require(
            !mounts.lines().any(|m| Path::new(m).starts_with(PGDATA)),
            "mounted PGDATA subtree",
        )?;
        let size =
            self.host.out(
                self.host
                    .cmd("du")
                    .args(["--apparent-size", "-s", "-B1", PGDATA]),
            )?;
        let size = size
            .split_ascii_whitespace()
            .next()
            .ok_or("primary size unavailable")?
            .parse::<u64>()?;
        if warm {
            require(
                free_bytes(Path::new(PGDATA))? > size.saturating_add(2 * GIB),
                "primary preseed space",
            )?;
        }
        let timeout = self.host.remaining()?.min(Duration::from_secs(900));
        let mut proc = self
            .host
            .cmd("rsync")
            .args(["-ax", "--numeric-ids", "--delete", "--checksum", "--fsync"])
            .arg(fmt(format_args!("{PGDATA}/"))?)
            .arg(fmt(format_args!("{}/", self.cold.display()))?)
            .env("PATH", SAFE_PATH)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        let outcome = (|| -> R<()> {
            let start = Instant::now();
            loop {
                if let Some(status) = proc.try_wait()? {
                    return require(
                        status.success() || (warm && status.code() == Some(24)),
                        "preparing primary data copy",
                    );
                }
                self.host.remaining()?;
                require(
                    free_bytes(Path::new(PGDATA))? > 2 * GIB,
                    "primary disk reserve",
                )?;
                require(start.elapsed() < timeout, "primary copy timeout")?;
                thread::sleep(Duration::from_millis(150));
            }
        })();
        if outcome.is_err() && proc.try_wait()?.is_none() {
            crate::common::stop_group(&mut proc);
        }
        outcome
    }

    fn control_state(&self, data: &Path) -> R<String> {
        let info = self.host.out(
            self.host
                .cmd(child(Path::new(TOOLS), "pg_controldata")?)
                .arg(data),
        )?;
        let state = info
            .lines()
            .find_map(|line| line.strip_prefix("Database cluster state:"))
            .ok_or("missing cluster state")?;
        owned(state.trim())
    }

    pub(super) fn migrate(&mut self, copy: &mut Recovery) -> R<()> {
        copy.seal(self)?;
        self.event("exact recovery point verified")?;
        self.pg_stopped = true;
        self.host
            .run(self.host.cmd("systemctl").args(["stop", PG]))?;
        require(
            self.control_state(Path::new(PGDATA))? == "shut down",
            "original primary clean shutdown",
        )?;
        self.copy_cold(false)?;
        crate::physical::check_no_links(&self.cold)?;
        require(
            self.control_state(&self.cold)? == "shut down"
                && !child(&self.cold, "postmaster.pid")?.try_exists()?,
            "candidate data copy consistency",
        )?;
        if self.isolation_change {
            service_database::add_hba(&child(&self.cold, "pg_hba.conf")?)?;
        }
        let unit = fmt(format_args!("td-mta-{}-candidate-db", name(&self.work)?))?;
        let trial = (|| -> R<()> {
            let job = fmt(format_args!("td-mta-{}.service", name(&self.work)?))?;
            let mut cmd = self.host.cmd("systemd-run");
            cmd.args(["--quiet", "--collect"])
                .arg(fmt(format_args!("--unit={unit}"))?)
                .arg("--property=Type=exec")
                .arg(fmt(format_args!("--property=BindsTo={job}"))?)
                .arg(fmt(format_args!("--property=After={job}"))?)
                .args([
                    "--property=User=td-mta",
                    "--property=Group=td-mta",
                    "--property=PrivateNetwork=yes",
                    "--property=PrivateIPC=yes",
                    "--property=ProtectSystem=strict",
                    "--property=ProtectHome=yes",
                    "--property=NoNewPrivileges=yes",
                    "--property=KillSignal=SIGINT",
                ])
                .arg(fmt(format_args!(
                    "--property=BindPaths={}:{PGDATA}",
                    self.cold.display()
                ))?)
                .arg(fmt(format_args!("--property=ReadWritePaths={PGDATA}"))?);
            if self.has_service_grants() {
                cmd.arg(fmt(format_args!(
                    "--property=ReadWritePaths={}/socket",
                    copy.runtime.display()
                ))?);
            }
            cmd.arg(child(Path::new(TOOLS), "postgres")?)
                .args(["-D", PGDATA, "-c"])
                .arg(fmt(format_args!("config_file={MAIL}/postgresql.conf"))?)
                .arg("-c")
                .arg(fmt(format_args!("data_directory={PGDATA}"))?)
                .args(["-c", "archive_mode=off", "-c", "primary_conninfo="]);
            if self.has_service_grants() {
                for (setting, file) in [
                    ("unix_socket_directories", "socket"),
                    ("hba_file", "admin-hba"),
                    ("ident_file", "admin-ident"),
                ] {
                    cmd.arg("-c").arg(fmt(format_args!(
                        "{setting}={}/{file}",
                        copy.runtime.display()
                    ))?);
                }
            } else {
                cmd.args(["-c", "unix_socket_directories="]);
            }
            self.host.run(&mut cmd)?;
            let pid = self.pid(&unit)?;
            require(
                fs::metadata(fmt(format_args!("/proc/{pid}/ns/net"))?)?.ino()
                    != fs::metadata("/proc/self/ns/net")?.ino(),
                "candidate database namespace",
            )?;
            let ns = [
                owned("nsenter")?,
                owned("--target")?,
                fmt(format_args!("{pid}"))?,
                owned("--net")?,
                owned("--")?,
            ];
            self.wait(15, "private candidate database startup", || {
                self.ready_db(&ns)
            })?;
            self.event("migrating fenced candidate database")?;
            self.upgrade(&ns, copy)
        })();
        let cleanup = Host::default();
        cleanup.recover();
        let stop = cleanup.exec(
            cleanup.cmd("systemctl").args(["stop", &unit]),
            &[],
            Duration::from_secs(120),
            false,
        );
        trial?;
        stop?;
        require(
            self.control_state(&self.cold)? == "shut down",
            "candidate shutdown",
        )?;
        fs::rename(PGDATA, &self.original)?;
        fs::rename(&self.cold, PGDATA)?;
        self.host.run(self.host.cmd("sync").args(["-f", PGDATA]))?;
        self.host
            .run(self.host.cmd("systemctl").args(["start", PG]))?;
        self.wait(15, "candidate database startup", || self.ready_db(&[]))?;
        self.check_source()?;
        let target = uint(
            self.plan.as_ref().ok_or("missing migration plan")?,
            "target",
        )?;
        require(
            self.sql(
                "owner",
                "SELECT max(version) FROM td_mta.schema_migrations",
                &[],
                false,
            )?
            .parse::<u64>()?
                == target,
            "live target ledger",
        )
    }
}

fn release_member(entry: &str) -> R<(u8, &str)> {
    let (kind, relative) = entry.split_at_checked(1).ok_or("release inventory entry")?;
    require(
        (kind == "d" || kind == "f")
            && !relative.is_empty()
            && relative.len() <= 4096
            && Path::new(relative)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            && !relative
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == ".."),
        "invalid release member",
    )?;
    Ok((if kind == "d" { b'd' } else { b'f' }, relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_inventory_only_admits_normal_relative_files_and_directories() {
        for valid in ["dnotices", "fnotices/LICENSE", "ftd-mta"] {
            assert!(release_member(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "f",
            "f/etc/passwd",
            "fnotices/../../x",
            "f./binary",
            "fnotices//license",
            "ltd-mta",
            "pnotices/special",
            "dtrailing/",
        ] {
            assert!(release_member(invalid).is_err(), "{invalid}");
        }
    }
}
