//! A host whose PostgreSQL still uses loopback TLS leaves it in one order:
//! a release that admits the group-shared socket while still reading TLS
//! connection files (the stepping stone), then `--postgres-socket`, then
//! main. Every other operation there refuses and names the step that is
//! next. The boundaries are main commits, checked by ancestry.
use super::{verdict, Local, Options, Refusal};
use crate::{
    activate::{redacted_runtime_tls, FENCE, REDACT_RUNTIME, RUNTIME_CONNECTION},
    common::{child, fmt, owned, require},
    target::HostConfig,
    R,
};
use std::path::{Path, PathBuf};

/// daceb7309 "td-fs, td-mta: admit a group-shared PostgreSQL socket": the
/// first main commit whose release connects through the socket
/// `--postgres-socket` installs (`td_fs::check_server_socket`).
pub(super) const GROUP_SOCKET: &str = "daceb73094ab5709a5bbc747159e8595080c1267";
/// The parent of c8f8e087d "td-mta: reach PostgreSQL only over its Unix
/// socket; delete TCP+TLS": the last main commit whose release still
/// reads loopback TLS connection files.
pub(super) const LAST_TCP_TLS: &str = "290c6de7d4f370e85721b970d4d2c17344423aae";
/// The last main commit whose controller activates a release on a
/// loopback-TLS host; its child 83a4654dd made activation refuse one. Its
/// controller sources are unchanged since 6cdb77363.
pub(super) const TLS_CONTROLLER: &str = "daceb73094ab5709a5bbc747159e8595080c1267";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    Deploy,
    Socket,
    Binaries,
    ToDebian,
    Retire,
    Recover,
    Rollback,
    Converge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    Proceed,
    /// Deploy the stepping stone with the loopback-TLS controller.
    SteppingStone,
    /// The stepping stone comes first.
    SteppingStoneFirst,
    /// `--postgres-socket` comes first.
    SocketFirst,
    /// A normal deploy, leaving the stepping stone, comes first.
    MainFirst,
}

/// What `operation` may do given the host's transport (`tls`) and whether
/// the live release contains GROUP_SOCKET (`admits`) or is LAST_TCP_TLS or
/// older (`stone`).
pub(super) fn step(operation: Operation, tls: bool, admits: bool, stone: bool) -> Step {
    match (tls, operation) {
        (true, Operation::Deploy) if !admits => Step::SteppingStone,
        (true, _) if !admits => Step::SteppingStoneFirst,
        (true, Operation::Socket) => Step::Proceed,
        (true, _) => Step::SocketFirst,
        // The binary switch's long rehearsal and window belong to the
        // release the host keeps; returning to Debian stays open. The
        // stone predates the policy convergence reads from the live
        // release.
        (false, Operation::Binaries | Operation::Converge) if stone => Step::MainFirst,
        (false, _) => Step::Proceed,
    }
}

fn operation(opts: &Options<'_>) -> Operation {
    if opts.socket {
        Operation::Socket
    } else if opts.to_debian {
        Operation::ToDebian
    } else if opts.binaries {
        Operation::Binaries
    } else if opts.retire {
        Operation::Retire
    } else if opts.recover {
        Operation::Recover
    } else if opts.rollback {
        Operation::Rollback
    } else if opts.converge {
        Operation::Converge
    } else {
        Operation::Deploy
    }
}

/// The two boundaries a stepping stone lies between, inclusive.
pub(super) struct Range<'a> {
    pub(super) admits: &'a str,
    pub(super) last_tls: &'a str,
}

pub(super) const RANGE: Range<'static> = Range {
    admits: GROUP_SOCKET,
    last_tls: LAST_TCP_TLS,
};

/// `td-mta/deploy` with `flags` and the operator's `--config`.
fn argv<'a>(flags: &[&'a str], config: Option<&'a str>) -> R<Vec<&'a str>> {
    let mut out = Vec::new();
    td_alloc::push(&mut out, "td-mta/deploy")?;
    td_alloc::extend_from_slice(&mut out, flags)?;
    if let Some(config) = config {
        td_alloc::extend_from_slice(&mut out, &["--config", config])?;
    }
    Ok(out)
}

/// The refusal, naming `next` if any, whose outcome line `run` writes.
fn refuse<T>(error: &str, next: Option<&[&str]>) -> R<T> {
    let next = match next {
        Some(argv) => {
            let mut words = Vec::new();
            for word in argv {
                td_alloc::push(&mut words, owned(word)?)?;
            }
            Some(words)
        }
        None => None,
    };
    Err(Refusal {
        error: owned(error)?,
        next,
    }
    .into())
}

/// The command a stepping-stone deploy needs: `--skip-tests` when no
/// stepping stone has a passing verdict here.
pub(super) fn stone_flags(verified: bool) -> &'static [&'static str] {
    if verified {
        &["--irreversible"]
    } else {
        &["--skip-tests", "--irreversible"]
    }
}

const NO_VERDICT: &str = "no stepping-stone revision (a first-parent origin/main revision after the live release, from daceb7309 through 290c6de7d) has a passing main-integration verdict on this machine, and the runner records verdicts only for the newest main head: deploy the newest stepping stone without one by adding --skip-tests";
const NO_STONE: &str = "PostgreSQL still uses loopback TLS and no first-parent origin/main revision from daceb7309 through 290c6de7d descends from the live release, so no stepping stone can follow it";
const IRREVERSIBLE: &str = "the stepping stone runs the loopback-TLS generation's controller, which migrates the database without asking: add --irreversible";
const FENCED: &str = "a deployment fence is held on a host still on loopback TLS: its job (the stepping stone or the socket cutover) is outside what --recover models; the fence file names it, and its receipt what it changed (td-mta/PRODUCTION.md, \"Leaving loopback TLS\")";
const RUNNING: &str = "a deployment job is still running on a host still on loopback TLS (its unit is printed above): wait for it to finish, following its journal, then run td-mta/deploy again; recover nothing by hand while it runs";
const STONE_FIRST: &str = "PostgreSQL still uses loopback TLS and the live release predates group-socket admission, so its isolated services could not reach the socket --postgres-socket installs: deploy the stepping-stone release first";
const SOCKET_FIRST: &str =
    "PostgreSQL still uses loopback TLS: move it to the private socket first";
const MAIN_FIRST: &str = "the live release is the loopback-TLS stepping stone: deploy main before switching the PostgreSQL binaries or converging the configuration";
const MAIN_UNVERIFIED: &str = "the live release is the loopback-TLS stepping stone: deploy main before switching the PostgreSQL binaries or converging the configuration; no main revision after 290c6de7d has a passing main-integration verdict here, so wait for the runner or deploy the tip untested with --skip-tests";

/// Which stepping stones exist after the live release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stones {
    None,
    Unverified,
    Verified,
}

/// What the launcher read before deciding.
pub(super) struct Facts {
    pub(super) operation: Operation,
    pub(super) skip: bool,
    pub(super) irreversible: bool,
    pub(super) tls: bool,
    pub(super) fenced: bool,
    /// A `td-mta-deploy-*` unit is active (read only when fenced).
    pub(super) running: bool,
    /// The live release contains GROUP_SOCKET.
    pub(super) admits: bool,
    /// The live release is a stepping stone (read on the socket, for a
    /// deploy, the binary switch and convergence).
    pub(super) stone_live: bool,
    /// Read only for a stepping-stone step.
    pub(super) stones: Stones,
    /// A main revision after LAST_TCP_TLS has a verdict (read under the
    /// stone).
    pub(super) main_verified: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Proceed,
    Stone,
    /// The error, and the flags of the `td-mta/deploy` that comes next.
    Refuse(&'static str, Option<&'static [&'static str]>),
}

pub(super) fn decide(f: &Facts) -> Decision {
    if f.tls && f.fenced {
        // A running job holds its own fence; once it has died, each step
        // would refuse in turn and name the other.
        return Decision::Refuse(if f.running { RUNNING } else { FENCED }, None);
    }
    let stepping = match step(f.operation, f.tls, f.admits, f.stone_live) {
        Step::Proceed => return Decision::Proceed,
        Step::SocketFirst => return Decision::Refuse(SOCKET_FIRST, Some(&["--postgres-socket"])),
        Step::MainFirst if f.main_verified => return Decision::Refuse(MAIN_FIRST, Some(&[])),
        Step::MainFirst => return Decision::Refuse(MAIN_UNVERIFIED, Some(&["--skip-tests"])),
        Step::SteppingStoneFirst => false,
        Step::SteppingStone => true,
    };
    match f.stones {
        Stones::None => Decision::Refuse(NO_STONE, None),
        stones if !stepping => {
            Decision::Refuse(STONE_FIRST, Some(stone_flags(stones == Stones::Verified)))
        }
        Stones::Unverified if !f.skip => Decision::Refuse(NO_VERDICT, Some(stone_flags(false))),
        // Without --skip-tests the stone has its verdict.
        _ if !f.irreversible => Decision::Refuse(IRREVERSIBLE, Some(stone_flags(!f.skip))),
        _ => Decision::Stone,
    }
}

/// The stepping stone `run` deploys with TLS_CONTROLLER, or `None` when
/// the operation proceeds as requested; a refusal is a `Refusal`, whose
/// one outcome line `run` writes for every mode, convergence included.
pub(super) fn plan(
    local: &Local,
    opts: &Options<'_>,
    site: &HostConfig,
    tip: &str,
    live: &str,
    verdicts: &Path,
) -> R<Option<(String, Option<String>)>> {
    let operation = operation(opts);
    let tls = local.loopback_tls()?;
    let fenced = tls && local.fenced()?;
    let running = fenced && local.running_jobs()?;
    let admits = local.is_ancestor(GROUP_SOCKET, live)?;
    let stone_live =
        !tls && matches!(
            operation,
            Operation::Deploy | Operation::Binaries | Operation::Converge
        ) && admits
            && local.is_ancestor(live, LAST_TCP_TLS)?;
    // Main proper: a verified revision that is not itself a stepping stone.
    let main_verified = stone_live
        && match local.verified_after(tip, live, verdicts)? {
            Some((rev, _)) => !local.is_ancestor(&rev, LAST_TCP_TLS)?,
            None => false,
        };
    let (mut verified, mut newest) = (None, None);
    if tls && !admits && !fenced {
        verified = local.stepping_stone(tip, live, false, verdicts, &RANGE)?;
        if verified.is_none() || opts.skip {
            newest = local.stepping_stone(tip, live, true, verdicts, &RANGE)?;
        }
    }
    let facts = Facts {
        operation,
        skip: opts.skip,
        irreversible: opts.irreversible,
        tls,
        fenced,
        running,
        admits,
        stone_live,
        stones: match (&verified, &newest) {
            (Some(_), _) => Stones::Verified,
            (None, Some(_)) => Stones::Unverified,
            (None, None) => Stones::None,
        },
        main_verified,
    };
    match decide(&facts) {
        Decision::Proceed => {
            if operation == Operation::Deploy && stone_live && !main_verified && !opts.skip {
                println!("The live release is a loopback-TLS stepping stone and no main revision after 290c6de7d has a passing verdict here: this deploy stays in the stepping-stone range. --skip-tests deploys the tip untested.");
            }
            Ok(None)
        }
        Decision::Refuse(error, next) => {
            let next = match next {
                Some(flags) => Some(argv(flags, opts.config)?),
                None => None,
            };
            refuse(error, next.as_deref())
        }
        Decision::Stone => {
            require(
                local.is_ancestor(TLS_CONTROLLER, tip)?,
                "the loopback-TLS controller revision is not on fetched main",
            )?;
            local.tls_controller_host(site)?;
            let stone = if opts.skip { newest } else { verified };
            let stone = stone.ok_or("missing stepping stone")?;
            println!(
                "PostgreSQL still uses loopback TLS: the stepping stone {} goes first, under the controller of {TLS_CONTROLLER}",
                stone.0
            );
            Ok(Some(stone))
        }
    }
}

/// `built`, the TLS controller's build, or a refusal saying what it needs.
pub(super) fn built(built: R<PathBuf>) -> R<PathBuf> {
    built.or_else(|error| {
        refuse(
            &fmt(format_args!(
                "building the loopback-TLS controller ({TLS_CONTROLLER}) failed ({error}); nothing reached the host. Its locked crates, tokio-postgres among them, are no longer main's: if Cargo found one missing, run cargo fetch --locked --manifest-path td-mta/Cargo.toml in a checkout of that revision, then deploy again"
            ))?,
            None,
        )
    })
}

/// The host TLS_CONTROLLER is written for, from its own sources: host
/// name, build account and source mirror, as `host.json` names them. Its
/// fixed public name, which its probes verify on the listeners'
/// certificate, may be any of that certificate's names, so is not read.
pub(super) fn controller_host(activate: &str, release: &str) -> R<[String; 3]> {
    fn quoted<'a>(text: &'a str, after: &str) -> R<&'a str> {
        let mut found = text.lines().filter_map(|line| {
            line.split_once(after)
                .and_then(|(_, rest)| rest.split_once('"'))
                .map(|(value, _)| value)
        });
        match (found.next(), found.next()) {
            (Some(value), None) if !value.is_empty() => Ok(value),
            _ => Err("unrecognized loopback-TLS controller sources".into()),
        }
    }
    Ok([
        owned(quoted(activate, "kernel/hostname\")?.trim() == \"")?)?,
        owned(quoted(release, "const BUILD_USER: &str = \"")?)?,
        owned(quoted(release, "const MIRROR: &str = \"")?)?,
    ])
}

impl Local {
    /// Whether the host's PostgreSQL still uses loopback TLS: the runtime
    /// connection file the controller reads, its password removed on the
    /// host, parsed as the controller parses it. The controller, which
    /// reads the whole file, still has the last word.
    pub(super) fn loopback_tls(&self) -> R<bool> {
        let location = self.sudo(&["sed", "-n", REDACT_RUNTIME, RUNTIME_CONNECTION])?;
        require(
            !location.is_empty() && !location.contains('\n'),
            "the host's runtime connection file is neither the loopback TLS nor the private socket form",
        )?;
        redacted_runtime_tls(&location)
    }

    /// Whether a deployment fence is held on the host.
    fn fenced(&self) -> R<bool> {
        let fence = Path::new(FENCE);
        let (Some(directory), Some(name)) = (
            fence.parent().and_then(Path::to_str),
            fence.file_name().and_then(|n| n.to_str()),
        ) else {
            return Err("deployment fence path".into());
        };
        // `find` fails, where `test` would only answer no, when sudo does.
        let found = self.sudo(&["find", directory, "-maxdepth", "1", "-name", name])?;
        Ok(!found.is_empty())
    }

    /// Whether a `td-mta-deploy-*` job unit is active, printing each.
    fn running_jobs(&self) -> R<bool> {
        let units = self.sudo(&[
            "systemctl",
            "list-units",
            "--plain",
            "--no-legend",
            "--state=active,activating,deactivating,reloading",
            "td-mta-deploy-*",
        ])?;
        let mut any = false;
        for unit in units.lines().filter_map(|l| l.split_whitespace().next()) {
            eprintln!("running deployment job: {unit}");
            any = true;
        }
        Ok(any)
    }

    /// TLS_CONTROLLER predates host.json and fixes its host in its sources;
    /// refuse before anything reaches the host unless host.json agrees.
    fn tls_controller_host(&self, site: &HostConfig) -> R<()> {
        let mut sources = Vec::new();
        for file in ["activate.rs", "activation_release.rs"] {
            let path = fmt(format_args!(
                "{TLS_CONTROLLER}:td-mta/src/bin/td-mta-deploy/{file}"
            ))?;
            td_alloc::push(
                &mut sources,
                self.host.out(self.git().args(["show", &path]))?,
            )?;
        }
        let [activate, release] = sources.as_slice() else {
            return Err("loopback-TLS controller sources".into());
        };
        let [host, account, mirror] = controller_host(activate, release)?;
        let differs = if site.host_name != host {
            "host_name"
        } else if site.build_account != account {
            "build_account"
        } else if site.mirror()? != Path::new(&mirror) {
            "build_home"
        } else {
            return Ok(());
        };
        refuse(
            &fmt(format_args!(
                "host.json's {differs} is not what the loopback-TLS controller ({TLS_CONTROLLER}) is written for, so it cannot deploy the stepping stone on this host"
            ))?,
            None,
        )
    }

    /// The newest first-parent revision of `tip` after `live` inside
    /// `range` with a passing verdict, or the newest one at all with
    /// `skip`; `None` when none qualifies.
    pub(super) fn stepping_stone(
        &self,
        tip: &str,
        live: &str,
        skip: bool,
        verdicts: &Path,
        range: &Range<'_>,
    ) -> R<Option<(String, Option<String>)>> {
        for boundary in [range.admits, range.last_tls] {
            require(
                self.is_ancestor(boundary, tip)?,
                "a stepping-stone boundary revision is not on fetched main",
            )?;
        }
        let revs = self.host.out(self.git().args([
            "rev-list",
            "--first-parent",
            tip,
            &fmt(format_args!("^{live}"))?,
        ]))?;
        // Newest first: the range starts at last_tls, and ends at the first
        // revision without admits.
        let mut inside = false;
        for rev in revs.lines() {
            super::revision(rev)?;
            inside |= rev == range.last_tls;
            if !inside || !self.is_ancestor(live, rev)? {
                continue;
            }
            if !self.is_ancestor(range.admits, rev)? {
                break;
            }
            if skip {
                return Ok(Some((owned(rev)?, None)));
            }
            if let Some(row) = verdict(&child(verdicts, rev)?)? {
                return Ok(Some((owned(rev)?, Some(row))));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "tests/stepping_stone.rs"]
mod tests;
