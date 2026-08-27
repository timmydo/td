#![forbid(unsafe_code)]
//! td-firstboot — mints this machine's identity into the persistent `/var`
//! subvolume, once, and reports on every later boot that it is unchanged.
//!
//! One read-only erofs image boots any number of machines, so a machine-id or SSH
//! host key baked into it would be shared by all of them. Those files live in
//! `/var`; the image's `/etc` reaches each through one reviewed symlink (the
//! `MUTABLE_ETC` table in `recipes/src/recipes/system-x86-64.rs`), which is what
//! keeps the tested read-only-`/etc` invariant true. This program owns only the
//! `/var` side — the image owns the names.
//!
//! Nothing here ever silently REPLACES an identity: a file that is present but
//! malformed is a hard error, because minting a new one over a bad read cannot be
//! noticed from outside. The Ed25519 key comes from OpenSSH `ssh-keygen`, the one
//! program in the image that already has the required key implementation, so this crate
//! stays dependency-free `std` with no crypto of its own.

mod machineid;
mod mounts;

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The markers the qemu boot oracle greps for. DUPLICATED as
/// `TD_FIRSTBOOT_NEW_MARKER` / `TD_FIRSTBOOT_STABLE_MARKER` /
/// `TD_FIRSTBOOT_HOST_KEY_PREFIX` in `recipes/src/ladder.rs` — this crate is built
/// from its own source, so it cannot share the consts. The td-firstboot recipe's
/// unit tests read these literals back out of this file and assert they match, so
/// the two copies cannot drift.
const NEW_MARKER: &str = "TD-FIRSTBOOT-NEW-OK";
const STABLE_MARKER: &str = "TD-FIRSTBOOT-STABLE-OK";
const HOST_KEY_PREFIX: &str = "TD-FIRSTBOOT-HOSTKEY ";

/// Where per-machine state lives. The recipe's `MUTABLE_ETC` table points every
/// persistent `/etc` symlink at this directory, and its unit tests read this
/// literal back out of this file to prove the two agree.
const DEFAULT_STATE_DIR: &str = "/var/lib/td";

/// The OpenSSH key utility used for generation and fingerprinting.
const DEFAULT_KEYGEN: &str = "/bin/ssh-keygen";

/// Paths relative to the state dir. Each is one entry in the recipe's table.
const MACHINE_ID: &str = "machine-id";
const HOST_KEY: &str = "ssh/ssh_host_ed25519_key";
const HOST_KEY_PUB: &str = "ssh/ssh_host_ed25519_key.pub";
const AUTHORIZED_KEYS: &str = "ssh/authorized_keys";

/// Shipped into a fresh `authorized_keys` so the file exists (the daemon reads it
/// on every connection) while authorizing nobody.
const AUTHORIZED_KEYS_HEADER: &str = "\
# td OpenSSH authorized_keys - one public key per line.\n\
# Empty => deny all. This file is per-machine state under /var, reached through\n\
# the /etc/ssh/authorized_keys symlink; adding a key here grants ROOT-equivalent\n\
# admin access to this machine and needs no image rebuild.\n";

/// Did this boot have to create the thing, or was it already there? One `Created`
/// anywhere makes the whole run a first boot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Created,
    Present,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Created => "created",
            Outcome::Present => "present",
        }
    }
}

struct Config {
    state: PathBuf,
    keygen: String,
    /// Whether to insist the state directory is on a writable, non-volatile
    /// filesystem. On by default; `--state-dir` turns it off because an
    /// operator-named directory carries no implied mount to check.
    require_persistent: bool,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    match run(args.get(1..).unwrap_or(&[])) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            emit_err(&format!("td-firstboot: {message}\n{}", usage()));
            ExitCode::from(2)
        }
        Err(Failure::Failed(message)) => {
            emit_err(&format!("td-firstboot: {message}\n"));
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug)]
enum Failure {
    /// Exit 2, the usage convention td-util and td-init already follow.
    Usage(String),
    Failed(String),
}

fn usage() -> String {
    format!(
        "usage: td-firstboot [provision] [--state-dir DIR] [--keygen PROGRAM]\n  \
         provisions this machine's identity under {DEFAULT_STATE_DIR}: {MACHINE_ID}, \
         {HOST_KEY}(.pub), {AUTHORIZED_KEYS}\n"
    )
}

fn run(args: &[String]) -> Result<(), Failure> {
    let config = match parse(args)? {
        Invocation::Help => return emit(&usage()).map_err(Failure::Failed),
        Invocation::Provision(config) => config,
    };
    let plan = Plan::of(&config);

    // The mount point the state dir lives on, when we checked for one. It bounds
    // the directory fsyncs below: there is no point (and on the read-only erofs
    // root, no ability) to fsync above it.
    let boundary = if config.require_persistent {
        Some(check_persistent(&config.state)?)
    } else {
        emit_err(&format!(
            "td-firstboot: state dir {} given explicitly; not checking that it is a \
             persistent mount\n",
            config.state.display()
        ));
        None
    };

    // 0755 on the state root, 0711 on the key directory: traversable but NOT
    // listable. 0700 would be tighter and wrong — the public host key is meant to
    // be readable (an operator reads its fingerprint, and /etc/rootcheck proves an
    // unprivileged read of the .pub succeeds while the private key's fails), and
    // without the directory's x bit no unprivileged process can reach either file
    // whatever their own modes say. Secrecy here is the FILE modes' job: 0600 on
    // the private key and on authorized_keys. Both directories are created by this
    // process running as root at sysinit, so they are root-owned with no chown.
    make_dir(&config.state, 0o755)?;
    make_dir(&plan.key_dir, 0o711)?;
    // `write_durably` fsyncs each file's OWN directory, which persists that file's
    // entry — but not the entries for the directories we just created. On a fresh
    // @var both /var/lib and /var/lib/td are new, so without this a power loss
    // right after the marker below could lose the whole tree and mint a different
    // machine on the next boot.
    sync_directories(&plan.key_dir, boundary.as_deref())?;

    let machine_id = provision_machine_id(&plan.machine_id)?;
    let (host_key, fingerprint) = provision_host_key(&config, &plan)?;
    let authorized = provision_authorized_keys(&plan.authorized_keys)?;

    // One greppable line per managed file, so a console log says which of them
    // this boot had to create.
    emit_err(&format!(
        "td-firstboot: machine-id {}\ntd-firstboot: host key {} {fingerprint}\n\
         td-firstboot: authorized_keys {}\n",
        machine_id.name(),
        host_key.name(),
        authorized.name(),
    ));

    let first_boot = [machine_id, host_key, authorized].contains(&Outcome::Created);
    emit(&format!(
        "{HOST_KEY_PREFIX}{fingerprint}\n{}\n",
        if first_boot {
            NEW_MARKER
        } else {
            STABLE_MARKER
        }
    ))
    .map_err(Failure::Failed)
}

/// What an argv asked for.
enum Invocation {
    Help,
    Provision(Config),
}

fn parse(args: &[String]) -> Result<Invocation, Failure> {
    let mut state: Option<PathBuf> = None;
    let mut keygen = DEFAULT_KEYGEN.to_string();
    let mut rest = args;
    // `provision` is accepted (and is the default) so the inittab line can name
    // what it does, and so a second mode could be added without changing it.
    if let Some(first) = rest.first() {
        if first == "provision" {
            rest = rest.get(1..).unwrap_or(&[]);
        }
    }
    let mut index = 0;
    while let Some(flag) = rest.get(index) {
        match flag.as_str() {
            // Every flag that does not end the parse takes a value, so the stride
            // below is unconditional.
            "--state-dir" | "--keygen" => {
                let Some(value) = rest.get(index + 1) else {
                    return Err(Failure::Usage(format!("flag `{flag}` needs a value")));
                };
                if flag == "--state-dir" {
                    state = Some(PathBuf::from(value));
                } else {
                    keygen = value.clone();
                }
            }
            "-h" | "--help" => return Ok(Invocation::Help),
            other => return Err(Failure::Usage(format!("unknown argument `{other}`"))),
        }
        index += 2;
    }
    Ok(Invocation::Provision(Config {
        require_persistent: state.is_none(),
        state: state.unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        keygen,
    }))
}

/// Every path this program manages, derived once.
struct Plan {
    key_dir: PathBuf,
    machine_id: PathBuf,
    host_key: PathBuf,
    host_key_pub: PathBuf,
    authorized_keys: PathBuf,
}

impl Plan {
    fn of(config: &Config) -> Plan {
        let host_key = config.state.join(HOST_KEY);
        Plan {
            // The key files all sit in one subdirectory; deriving it from the key
            // path keeps the two from disagreeing.
            key_dir: host_key
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| config.state.clone()),
            machine_id: config.state.join(MACHINE_ID),
            host_key,
            host_key_pub: config.state.join(HOST_KEY_PUB),
            authorized_keys: config.state.join(AUTHORIZED_KEYS),
        }
    }
}

/// Refuse to provision onto a filesystem that cannot keep what it is given. See
/// `mounts` for why this is worth a check of its own.
fn check_persistent(state: &Path) -> Result<PathBuf, Failure> {
    let mounts = std::fs::read_to_string("/proc/mounts")
        .map_err(|e| Failure::Failed(format!("read /proc/mounts: {e}")))?;
    let path = state.to_string_lossy();
    let Some(filesystem) = mounts::covering(&mounts, &path) else {
        return Err(Failure::Failed(format!(
            "no mount in /proc/mounts covers {path}, so there is nowhere to put this \
             machine's identity"
        )));
    };
    if filesystem.volatile() {
        return Err(Failure::Failed(format!(
            "{path} is on {} mounted at {} - a machine identity written there is \
             regenerated on every boot, which is not an identity. Mount persistent \
             storage there first",
            filesystem.fstype, filesystem.point
        )));
    }
    if !filesystem.writable {
        return Err(Failure::Failed(format!(
            "{path} is on {} mounted read-only at {} - the persistent subvolume is \
             not mounted",
            filesystem.fstype, filesystem.point
        )));
    }
    emit_err(&format!(
        "td-firstboot: state {path} on {} ({})\n",
        filesystem.fstype, filesystem.point
    ));
    Ok(PathBuf::from(filesystem.point))
}

/// fsync `deepest` and every ancestor up to and including `boundary` (the mount
/// point), so the directory entries themselves are on disk and not merely in page
/// cache. Without a boundary — an operator-named state dir — stop one level up,
/// which is as far as this program can claim anything about.
///
/// Cheap: on every boot after the first these are unchanged directories, and there
/// are four of them.
fn sync_directories(deepest: &Path, boundary: Option<&Path>) -> Result<(), Failure> {
    let mut directory = Some(deepest);
    // The chain is /var/lib/td/ssh -> /var; the cap is a backstop against a
    // pathological path rather than an expected limit.
    for _ in 0..32 {
        let Some(path) = directory else {
            return Ok(());
        };
        std::fs::File::open(path)
            .and_then(|handle| handle.sync_all())
            .map_err(|e| Failure::Failed(format!("fsync {}: {e}", path.display())))?;
        if Some(path) == boundary {
            return Ok(());
        }
        directory = match path.parent() {
            // Never walk past the boundary, and never to `/` — on td that is the
            // read-only erofs root, which has nothing to flush.
            Some(parent) if parent != Path::new("/") && !parent.as_os_str().is_empty() => {
                Some(parent)
            }
            _ => None,
        };
        if boundary.is_none() {
            return Ok(());
        }
    }
    Ok(())
}

fn provision_machine_id(path: &Path) -> Result<Outcome, Failure> {
    match read_optional(path)? {
        Some(text) => {
            let metadata = std::fs::metadata(path)
                .map_err(|e| Failure::Failed(format!("stat {}: {e}", path.display())))?;
            enforce_mode(path, &metadata, 0o444)?;
            machineid::validate(&text).map_err(|why| {
                Failure::Failed(format!(
                    "{}: {why}. Refusing to replace it - this machine's id is not \
                     something to discard over a bad read; move the file aside \
                     deliberately if it is unrecoverable",
                    path.display()
                ))
            })?;
            Ok(Outcome::Present)
        }
        None => {
            let mut bytes = [0u8; 16];
            read_entropy(&mut bytes)?;
            // 0444: everything on the machine may read its id, nothing should
            // rewrite it in place.
            write_durably(path, machineid::encode(&bytes).as_bytes(), 0o444)?;
            Ok(Outcome::Created)
        }
    }
}

/// Delegate Ed25519 generation and fingerprinting to OpenSSH. A complete existing
/// pair is preserved; an incomplete pair is a hard error rather than an invitation
/// to overwrite or silently repair the machine's identity.
fn provision_host_key(config: &Config, plan: &Plan) -> Result<(Outcome, String), Failure> {
    let private_exists = path_exists(&plan.host_key)?;
    let public_exists = path_exists(&plan.host_key_pub)?;
    let outcome = match (private_exists, public_exists) {
        (true, true) => Outcome::Present,
        (false, false) => {
            let output = std::process::Command::new(&config.keygen)
                .arg("-q")
                .arg("-t")
                .arg("ed25519")
                .arg("-N")
                .arg("")
                .arg("-C")
                .arg("td-openssh-host-key")
                .arg("-f")
                .arg(&plan.host_key)
                .output()
                .map_err(|e| {
                    Failure::Failed(format!(
                        "run `{}` to generate {}: {e} - this machine has no SSH host \
                         identity, so sshd will refuse to start",
                        config.keygen,
                        plan.host_key.display()
                    ))
                })?;
            require_keygen_success(config, "generate Ed25519 host key", &output)?;
            Outcome::Created
        }
        _ => {
            return Err(Failure::Failed(format!(
                "incomplete SSH host identity: private key {} is {}, public key {} is {}. \
                 Refusing to replace or reconstruct either file; restore the pair or move \
                 both aside deliberately",
                plan.host_key.display(),
                if private_exists { "present" } else { "missing" },
                plan.host_key_pub.display(),
                if public_exists { "present" } else { "missing" }
            )))
        }
    };
    // Trust nothing: the daemon that reads this key next has no way to report a
    // missing file except by refusing to start, so check here where the
    // diagnostic can still say which path is wrong.
    for (path, mode) in [(&plan.host_key, 0o600), (&plan.host_key_pub, 0o644)] {
        let metadata = std::fs::metadata(path).map_err(|e| {
            Failure::Failed(format!(
                "`{}` reported success but {} is not there: {e}",
                config.keygen,
                path.display()
            ))
        })?;
        enforce_mode(path, &metadata, mode)?;
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|e| Failure::Failed(format!("fsync {}: {e}", path.display())))?;
    }
    std::fs::File::open(&plan.key_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|e| Failure::Failed(format!("fsync {}: {e}", plan.key_dir.display())))?;

    // The public file is only a cache of what the private identity says. Parse the
    // private key and derive its public half before trusting either one: otherwise a
    // valid but mismatched .pub file would make us report a fingerprint the daemon
    // does not present, while a malformed private key would still look "stable".
    let output = std::process::Command::new(&config.keygen)
        .arg("-y")
        .arg("-P")
        .arg("")
        .arg("-f")
        .arg(&plan.host_key)
        .output()
        .map_err(|e| {
            Failure::Failed(format!(
                "run `{}` to derive the public key from {}: {e}",
                config.keygen,
                plan.host_key.display()
            ))
        })?;
    require_keygen_success(config, "derive Ed25519 public host key", &output)?;
    let derived = String::from_utf8(output.stdout).map_err(|e| {
        Failure::Failed(format!(
            "`{} -y` printed a non-UTF-8 public key for {}: {e}",
            config.keygen,
            plan.host_key.display()
        ))
    })?;
    let public = std::fs::read_to_string(&plan.host_key_pub)
        .map_err(|e| Failure::Failed(format!("read {}: {e}", plan.host_key_pub.display())))?;
    let derived_identity = ed25519_public_identity(
        &derived,
        &format!("public key derived from {}", plan.host_key.display()),
    )?;
    let recorded_identity = ed25519_public_identity(
        &public,
        &format!("recorded public key {}", plan.host_key_pub.display()),
    )?;
    if derived_identity != recorded_identity {
        return Err(Failure::Failed(format!(
            "SSH host key pair does not match: {} derives a different public key than {}. \
             Refusing to report or replace either identity",
            plan.host_key.display(),
            plan.host_key_pub.display()
        )));
    }

    let output = std::process::Command::new(&config.keygen)
        .arg("-l")
        .arg("-E")
        .arg("sha256")
        .arg("-f")
        .arg(&plan.host_key_pub)
        .output()
        .map_err(|e| {
            Failure::Failed(format!(
                "run `{}` to fingerprint {}: {e}",
                config.keygen,
                plan.host_key_pub.display()
            ))
        })?;
    require_keygen_success(config, "fingerprint Ed25519 host key", &output)?;
    let reply = String::from_utf8_lossy(&output.stdout);
    let mut words = reply.split_whitespace();
    let bits = words.next();
    let fingerprint = words.next();
    let algorithm = words.next_back();
    let valid = bits == Some("256")
        && fingerprint.is_some_and(|value| {
            value
                .strip_prefix("SHA256:")
                .is_some_and(|digest| !digest.is_empty())
        })
        && algorithm == Some("(ED25519)")
        && reply.lines().count() == 1;
    if !valid {
        return Err(Failure::Failed(format!(
            "`{} -l -E sha256` printed {reply:?}, expected one 256-bit Ed25519 SHA-256 \
             fingerprint line",
            config.keygen
        )));
    }
    let Some(fingerprint) = fingerprint else {
        return Err(Failure::Failed(format!(
            "`{} -l -E sha256` omitted the fingerprint",
            config.keygen
        )));
    };
    Ok((outcome, fingerprint.to_string()))
}

fn ed25519_public_identity(text: &str, source: &str) -> Result<(String, String), Failure> {
    let mut words = text.split_whitespace();
    let algorithm = words.next();
    let encoded = words.next();
    if algorithm != Some("ssh-ed25519")
        || encoded.is_none_or(str::is_empty)
        || text.lines().count() != 1
    {
        return Err(Failure::Failed(format!(
            "{source} is not one Ed25519 public-key line"
        )));
    }
    let Some(encoded) = encoded else {
        return Err(Failure::Failed(format!(
            "{source} omits its public-key encoding"
        )));
    };
    Ok(("ssh-ed25519".to_string(), encoded.to_string()))
}

fn path_exists(path: &Path) -> Result<bool, Failure> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Failure::Failed(format!(
            "inspect {} before provisioning: {error}",
            path.display()
        ))),
    }
}

fn require_keygen_success(
    config: &Config,
    operation: &str,
    output: &std::process::Output,
) -> Result<(), Failure> {
    if output.status.success() {
        return Ok(());
    }
    Err(Failure::Failed(format!(
        "`{}` failed to {operation} ({}): {}",
        config.keygen,
        output.status,
        String::from_utf8_lossy(&output.stderr).trim_end()
    )))
}

/// Create the file the daemon authorizes from, EMPTY. A fresh machine must
/// authorize nobody, and the daemon treats a missing file as deny-all too — but a
/// file that exists is one an operator can append to without first knowing where
/// it goes or what it may contain.
fn provision_authorized_keys(path: &Path) -> Result<Outcome, Failure> {
    match read_optional(path)? {
        Some(text) => {
            let keys = text
                .lines()
                .filter(|line| {
                    let line = line.trim();
                    !line.is_empty() && !line.starts_with('#')
                })
                .count();
            // Repair the mode every boot, not just at create. This file is a
            // root-equivalent grant: if anything ever widened it, a local user
            // could append their own key and the change would survive reboots.
            let metadata = std::fs::metadata(path)
                .map_err(|e| Failure::Failed(format!("stat {}: {e}", path.display())))?;
            enforce_mode(path, &metadata, 0o600)?;
            emit_err(&format!(
                "td-firstboot: {} authorizes {keys} key(s)\n",
                path.display()
            ));
            Ok(Outcome::Present)
        }
        None => {
            write_durably(path, AUTHORIZED_KEYS_HEADER.as_bytes(), 0o600)?;
            Ok(Outcome::Created)
        }
    }
}

/// Read a file that is expected to be absent on a first boot. Any error OTHER
/// than "not there" is propagated: a state file we cannot read is not a state file
/// that does not exist, and treating the two alike is how identity gets replaced.
fn read_optional(path: &Path) -> Result<Option<String>, Failure> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Failure::Failed(format!("read {}: {e}", path.display()))),
    }
}

/// 16 bytes from the kernel CSPRNG, read as an ordinary file so this crate needs
/// no RNG dependency.
///
/// `/dev/random`, NOT `/dev/urandom`: this runs as the fifth sysinit job, before
/// networking, and urandom will hand out bytes from a CRNG that is not yet seeded.
/// A fleet of identical VMs first-booting the same image would then mint correlated
/// identities — precisely what per-machine identity exists to prevent. Since Linux
/// 5.6 `/dev/random` blocks only until the CRNG is initialized and is otherwise
/// identical, which is the tradeoff worth taking for a key that lives as long as
/// the machine.
fn read_entropy(bytes: &mut [u8; 16]) -> Result<(), Failure> {
    std::fs::File::open("/dev/random")
        .and_then(|mut random| random.read_exact(bytes))
        .map_err(|e| Failure::Failed(format!("read {} bytes from /dev/random: {e}", bytes.len())))
}

fn make_dir(path: &Path, mode: u32) -> Result<(), Failure> {
    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
    {
        Ok(()) => {}
        Err(e) => {
            return Err(Failure::Failed(format!(
                "create {} (mode {mode:o}): {e}",
                path.display()
            )))
        }
    }
    // `recursive(true)` is silent when the directory already exists, so the mode
    // above only applies the first time. Repair it, so a directory an earlier
    // version created too permissively is fixed rather than trusted.
    let metadata = std::fs::metadata(path)
        .map_err(|e| Failure::Failed(format!("stat {}: {e}", path.display())))?;
    enforce_mode(path, &metadata, mode)
}

/// Reset a path's permission bits when they are not what this program requires.
/// Only on mismatch: the common case is a boot that changes nothing on persistent
/// storage.
fn enforce_mode(path: &Path, metadata: &std::fs::Metadata, mode: u32) -> Result<(), Failure> {
    if metadata.permissions().mode() & 0o7777 == mode {
        return Ok(());
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Failure::Failed(format!("chmod {} to {mode:o}: {e}", path.display())))
}

/// Write through a same-directory temporary and rename. The rename is atomic, so
/// no reader — and no interrupted first boot — can observe a half-written identity
/// file; and because a malformed one is a hard error rather than a regeneration,
/// "half-written" would otherwise be a machine that refuses to provision. `mode`
/// is applied at CREATE time so a 0600 file is never briefly world-readable, and
/// both the file and its directory are fsync'd: the boot that generated an
/// identity must not continue as if it had one that is still only in page cache.
fn write_durably(path: &Path, bytes: &[u8], mode: u32) -> Result<(), Failure> {
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut name = path.as_os_str().to_owned();
    name.push(".new");
    let temporary = PathBuf::from(name);

    // Unlink any leftover and create EXCLUSIVELY: reusing an existing temporary
    // would keep ITS mode (OpenOptions applies `mode` only when it creates the
    // file), which is how a 0600 file ends up 0644.
    match std::fs::remove_file(&temporary) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Failure::Failed(format!(
                "clear stale {}: {e}",
                temporary.display()
            )))
        }
    }
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(directory)?.sync_all()
    };
    write().map_err(|e| {
        Failure::Failed(format!(
            "write {} (mode {mode:o}) through {}: {e}",
            path.display(),
            temporary.display()
        ))
    })?;
    // `OpenOptions::mode` is modulated by the umask, which can only make the file
    // STRICTER — harmless for a private key, but machine-id must stay world-readable
    // to serve its purpose, and a restrictive inherited umask would quietly make it
    // root-only. Set the mode explicitly so none of these files depend on what umask
    // PID 1 happened to hand this job.
    let metadata = std::fs::metadata(path)
        .map_err(|e| Failure::Failed(format!("stat {}: {e}", path.display())))?;
    enforce_mode(path, &metadata, mode)
}

/// Markers to stdout. A closed reader is a clean exit, not a panic: `println!`
/// panics on a failed write and Rust leaves SIGPIPE ignored, so `td-firstboot |
/// head` would abort — which the no-panic rule forbids.
fn emit(text: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    match out.write_all(text.as_bytes()).and_then(|()| out.flush()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Diagnostics to stderr — at sysinit this IS the console log. A failure to report
/// a failure has nowhere left to go, so drop it rather than panic.
fn emit_err(text: &str) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(text.as_bytes()).and_then(|()| err.flush());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse` without the `Invocation` wrapper, for the argv shapes that must
    /// yield a provisioning config.
    fn config(args: &[&str]) -> Result<Config, Failure> {
        match parse(&args.iter().map(|a| (*a).to_string()).collect::<Vec<_>>())? {
            Invocation::Provision(config) => Ok(config),
            Invocation::Help => Err(Failure::Usage("asked for help".to_string())),
        }
    }

    #[test]
    fn the_default_run_provisions_the_image_paths_and_checks_the_mount() {
        for argv in [vec![], vec!["provision"]] {
            let config = config(&argv).unwrap();
            assert_eq!(config.state, PathBuf::from("/var/lib/td"));
            assert_eq!(config.keygen, "/bin/ssh-keygen");
            assert!(config.require_persistent);

            let plan = Plan::of(&config);
            assert_eq!(plan.key_dir, PathBuf::from("/var/lib/td/ssh"));
            assert_eq!(plan.machine_id, PathBuf::from("/var/lib/td/machine-id"));
            assert_eq!(
                plan.host_key,
                PathBuf::from("/var/lib/td/ssh/ssh_host_ed25519_key")
            );
            assert_eq!(
                plan.host_key_pub,
                PathBuf::from("/var/lib/td/ssh/ssh_host_ed25519_key.pub")
            );
            assert_eq!(
                plan.authorized_keys,
                PathBuf::from("/var/lib/td/ssh/authorized_keys")
            );
        }
    }

    /// An operator-named state dir has no implied mount, so the persistence check
    /// does not apply to it — and that relaxation must not leak into the default.
    #[test]
    fn an_explicit_state_dir_relocates_everything_and_drops_the_mount_check() {
        let config = config(&["--state-dir", "/srv/state", "--keygen", "/opt/ssh-keygen"]).unwrap();
        assert!(!config.require_persistent);
        assert_eq!(config.keygen, "/opt/ssh-keygen");
        let plan = Plan::of(&config);
        assert_eq!(plan.key_dir, PathBuf::from("/srv/state/ssh"));
        assert_eq!(plan.machine_id, PathBuf::from("/srv/state/machine-id"));
    }

    #[test]
    fn a_bad_argv_is_a_usage_error_not_a_partial_provision() {
        for argv in [
            vec!["--state-dir"],
            vec!["--keygen"],
            vec!["--nonesuch"],
            vec!["provision", "extra"],
            vec!["provision", "provision"],
        ] {
            assert!(
                matches!(config(&argv), Err(Failure::Usage(_))),
                "`td-firstboot {argv:?}` must be rejected as a usage error"
            );
        }
    }

    #[test]
    fn help_is_not_a_provisioning_run() {
        for argv in [vec!["-h"], vec!["--help"], vec!["provision", "--help"]] {
            let argv: Vec<String> = argv.iter().map(|a| (*a).to_string()).collect();
            assert!(matches!(parse(&argv), Ok(Invocation::Help)));
        }
    }

    #[test]
    fn the_authorized_keys_header_authorizes_nobody() {
        for line in AUTHORIZED_KEYS_HEADER.lines() {
            assert!(
                line.starts_with('#'),
                "a shipped authorized_keys line that is not a comment would grant access: {line:?}"
            );
        }
    }

    #[test]
    fn a_public_identity_is_one_ed25519_line_with_an_encoded_key() {
        assert_eq!(
            ed25519_public_identity("ssh-ed25519 AAAA comment with spaces\n", "fixture").unwrap(),
            ("ssh-ed25519".to_string(), "AAAA".to_string())
        );
        for invalid in [
            "",
            "ssh-ed25519",
            "ssh-rsa AAAA",
            "ssh-ed25519 AAAA\nssh-ed25519 BBBB\n",
        ] {
            assert!(
                ed25519_public_identity(invalid, "fixture").is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    /// The markers are a contract with the boot oracle: distinct, and neither one
    /// a prefix of the other (the oracle latches on substrings).
    #[test]
    fn the_markers_are_distinct_and_not_substrings_of_each_other() {
        assert_ne!(NEW_MARKER, STABLE_MARKER);
        assert!(!NEW_MARKER.contains(STABLE_MARKER));
        assert!(!STABLE_MARKER.contains(NEW_MARKER));
        assert!(!NEW_MARKER.contains(HOST_KEY_PREFIX));
        assert!(!STABLE_MARKER.contains(HOST_KEY_PREFIX));
        assert!(HOST_KEY_PREFIX.ends_with(' '), "the fingerprint follows it");
    }
}
