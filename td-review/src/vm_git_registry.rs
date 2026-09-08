//! Single-file VM key and branch authority, operated by the Git account.
use super::{
    absolute, bounded, branch_valid, instance_valid, Policy, Result, Session, LIMIT, MAX_BRANCHES,
};
use std::ffi::OsString;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Stdio;

pub(super) fn key_valid(encoded: &str) -> Result<()> {
    if encoded.len() != 68 {
        return Err("expected an ordinary Ed25519 public key".into());
    }
    let mut decoded = Vec::with_capacity(51);
    for [a, b, c, d] in encoded.as_bytes().as_chunks::<4>().0 {
        let sextet = |byte: u8| -> Result<u8> {
            match byte {
                b'A'..=b'Z' => Ok(byte - b'A'),
                b'a'..=b'z' => Ok(byte - b'a' + 26),
                b'0'..=b'9' => Ok(byte - b'0' + 52),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err("invalid public key encoding".into()),
            }
        };
        let (a, b, c, d) = (sextet(*a)?, sextet(*b)?, sextet(*c)?, sextet(*d)?);
        decoded.extend_from_slice(&[(a << 2) | (b >> 4), (b << 4) | (c >> 2), (c << 6) | d]);
    }
    if decoded.get(..19) != Some(b"\0\0\0\x0bssh-ed25519\0\0\0 ".as_slice()) {
        return Err("public key is not the Ed25519 wire format".into());
    }
    Ok(())
}

fn public_key(path: &Path) -> Result<String> {
    let text = String::from_utf8(bounded(File::open(path)?)?)?;
    let mut lines = text.lines();
    let line = lines.next().ok_or("empty public key file")?;
    if lines.next().is_some() {
        return Err("expected one public key line".into());
    }
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("ssh-ed25519") {
        return Err("expected an ordinary Ed25519 public key".into());
    }
    let key = words.next().ok_or("missing public key bytes")?;
    key_valid(key)?;
    Ok(key.into())
}

fn serialize(policy: &Policy) -> Result<String> {
    let mut text = format!(
        "TDVM-GIT-2\nrepository={}\ngit={}\npath={}\ndispatcher={}\n",
        policy.repository.display(),
        policy.git.display(),
        policy.path,
        policy.dispatcher.display()
    );
    for (id, key) in &policy.keys {
        text.push_str(&format!("key={id} {key}\n"));
    }
    for (branch, id) in &policy.branches {
        text.push_str(&format!("branch={id} {branch}\n"));
    }
    if text.len() as u64 > LIMIT {
        return Err("registry exceeds one MiB".into());
    }
    if Policy::parse(&text)? != *policy {
        return Err("registry does not round trip".into());
    }
    Ok(text)
}

fn lock(path: &Path) -> Result<File> {
    Policy::private_parent(path)?;
    let location = path
        .parent()
        .ok_or("missing policy parent")?
        .join(".td-vm-git.lock");
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&location)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let meta = fs::symlink_metadata(&location)?;
            if !meta.is_file() {
                return Err("registry lock is not a regular file".into());
            }
            OpenOptions::new().read(true).write(true).open(&location)?
        }
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    let named = fs::symlink_metadata(&location)?;
    if !named.is_file()
        || meta.uid() != fs::metadata("/proc/self")?.uid()
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || (meta.dev(), meta.ino()) != (named.dev(), named.ino())
    {
        return Err("untrusted or replaced registry lock".into());
    }
    file.try_lock()
        .map_err(|error| format!("registry writer is unavailable: {error}"))?;
    Ok(file)
}

fn publish(path: &Path, policy: &Policy) -> Result<()> {
    let text = serialize(policy)?;
    let parent = path.parent().ok_or("missing policy parent")?;
    let staging = Session::create(parent)?;
    let temporary = staging.0.join("policy");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn init(args: &[OsString]) -> Result<bool> {
    let [path, repository, git, tool_path, dispatcher] = args else {
        return Err("usage: td-vm-git init POLICY REPOSITORY GIT TOOL_PATH DISPATCHER".into());
    };
    let text = |value: &OsString| -> Result<String> {
        let text = value.to_str().ok_or("configuration must be UTF-8")?;
        if text.chars().any(char::is_control) {
            return Err("configuration contains a control character".into());
        }
        Ok(text.into())
    };
    let path = absolute(&text(path)?)?;
    let policy = Policy::parse(&format!(
        "TDVM-GIT-2\nrepository={}\ngit={}\npath={}\ndispatcher={}\n",
        text(repository)?,
        text(git)?,
        text(tool_path)?,
        text(dispatcher)?
    ))?;
    let dispatcher = fs::metadata(&policy.dispatcher)?;
    if !dispatcher.is_file() || dispatcher.mode() & 0o111 == 0 {
        return Err("dispatcher must be executable".into());
    }
    if policy.query(&["rev-parse", "--is-bare-repository"])? != "true" {
        return Err("VM origin must be bare".into());
    }
    let parent = path.parent().ok_or("missing policy parent")?;
    match DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let _lock = lock(&path)?;
    // Also commit a directory left by an interrupted earlier initialization.
    File::open(parent.parent().ok_or("missing registry ancestor")?)?.sync_all()?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("registry already exists; refusing to replace it".into()),
    }
    publish(&path, &policy)?;
    Ok(true)
}

fn reserve(policy: &mut Policy, id: &str, branch: &str) -> Result<Option<String>> {
    if !instance_valid(id) || !branch_valid(branch) || !policy.keys.contains_key(id) {
        return Err("reservation needs a registered instance and valid task branch".into());
    }
    if let Some(owner) = policy.branches.get(branch) {
        return if owner == id {
            Ok(None)
        } else {
            Err("branch belongs to another instance".into())
        };
    }
    if policy.branches.len() >= MAX_BRANCHES {
        return Err("maximum branch reservations reached".into());
    }
    let overlaps = |old: &str| {
        old == branch
            || old
                .strip_prefix(branch)
                .is_some_and(|rest| rest.starts_with('/'))
            || branch
                .strip_prefix(old)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    if policy.branches.keys().any(|old| overlaps(old)) {
        return Err("branch reservation overlaps another reservation".into());
    }
    let refs = policy.query(&["for-each-ref", "--format=%(refname)", "refs/heads/"])?;
    if refs
        .lines()
        .filter_map(|name| name.strip_prefix("refs/heads/"))
        .any(overlaps)
    {
        return Err(
            "branch already exists in the origin; explicit ownership transfer is required".into(),
        );
    }
    let status = policy
        .command()
        .args(["symbolic-ref", "--quiet", &format!("refs/heads/{branch}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?;
    if status.code() != Some(1) {
        return Err("new branch is symbolic or could not be inspected".into());
    }
    policy.branches.insert(branch.into(), id.into());
    Ok(Some(branch.into()))
}

fn claim_branch(policy: &Policy, branch: &str) -> Result<()> {
    let oid = policy.query(&["rev-parse", "--verify", "HEAD^{commit}"])?;
    if ![40, 64].contains(&oid.len()) || !oid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("origin HEAD did not resolve to a commit id".into());
    }
    let mut child = policy
        .command()
        .args(["update-ref", "--no-deref", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let input = match child.stdin.take() {
        Some(mut input) => writeln!(input, "create refs/heads/{branch} {oid}"),
        None => Err(io::Error::other("missing ref transaction input")),
    };
    let status = child.wait()?;
    input?;
    if !status.success() {
        return Err("Git branch claim failed; registry unchanged".into());
    }
    Ok(())
}

fn change(verb: &str, args: &[OsString]) -> Result<bool> {
    let (path, remaining) = args.split_first().ok_or("missing registry path")?;
    let path = Path::new(path);
    let _lock = lock(path)?;
    let mut policy = Policy::load(path)?;
    let before = serialize(&policy)?;
    let words: Vec<_> = remaining
        .iter()
        .map(|word| word.to_str().ok_or("arguments must be UTF-8"))
        .collect::<std::result::Result<_, _>>()?;
    let mut claim = None;
    match (verb, words.as_slice()) {
        ("enroll", [id, branch, key_path]) => {
            let key = public_key(Path::new(key_path))?;
            if !instance_valid(id) || policy.keys.iter().any(|(owner, registered)| owner != id && registered == &key) {
                return Err("invalid instance or public key already enrolled elsewhere".into());
            }
            if policy.keys.get(*id).is_some_and(|registered| registered != &key) {
                return Err("instance already has another key; revoke it explicitly first".into());
            }
            if !policy.keys.contains_key(*id) && policy.keys.len() >= MAX_BRANCHES {
                return Err("maximum enrolled keys reached".into());
            }
            policy.keys.insert((*id).into(), key);
            claim = reserve(&mut policy, id, branch)?;
        }
        ("reserve", [id, branch]) => claim = reserve(&mut policy, id, branch)?,
        ("revoke", [id]) => {
            if !instance_valid(id) {
                return Err("invalid instance id".into());
            }
            policy.keys.remove(*id);
            policy.branches.retain(|_, owner| owner != id);
        }
        _ => return Err("usage: td-vm-git enroll POLICY ID BRANCH PUBLIC_KEY | reserve POLICY ID BRANCH | revoke POLICY ID".into()),
    }
    if serialize(&policy)? != before {
        // Git's create transaction arbitrates with ordinary repository writers.
        // Never delete the ref on a later error: another writer may have used it.
        if let Some(branch) = claim {
            claim_branch(&policy, &branch)?;
        }
        publish(path, &policy)?;
    } else {
        // Complete durability after a prior rename succeeded but its directory
        // sync failed; an idempotent retry must not acknowledge unsynced state.
        File::open(path)?.sync_all()?;
        File::open(path.parent().ok_or("missing policy parent")?)?.sync_all()?;
    }
    Ok(true)
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn authorized_keys(args: &[OsString]) -> Result<bool> {
    let [path, kind, key] = args else {
        return Err("usage: td-vm-git authorized-keys POLICY KEY_TYPE KEY_BASE64".into());
    };
    let Some(key) = key.to_str() else {
        return Ok(true);
    };
    if kind != "ssh-ed25519" || key_valid(key).is_err() {
        return Ok(true);
    }
    let policy_path = path.to_str().ok_or("policy path must be UTF-8")?;
    let policy = Policy::load(Path::new(path))?;
    for (id, registered) in &policy.keys {
        if registered == key && policy.authorized(id, key) {
            let dispatcher = policy
                .dispatcher
                .to_str()
                .ok_or("dispatcher path must be UTF-8")?;
            let command = format!(
                "{} serve {} {id} {key}",
                quote(dispatcher),
                quote(policy_path)
            );
            // OpenSSH opt_dequote only unescapes backslash-double-quote;
            // doubling other backslashes would change the shell argv.
            let command = command.replace('"', "\\\"");
            writeln!(
                io::stdout().lock(),
                "restrict,command=\"{command}\" ssh-ed25519 {key} td-vm:{id}"
            )?;
        }
    }
    Ok(true)
}

pub(super) fn cli(args: &[OsString]) -> Option<Result<bool>> {
    let (verb, remaining) = args.split_first()?;
    match verb.to_str()? {
        "init" => Some(init(remaining)),
        "enroll" | "reserve" | "revoke" => Some(change(verb.to_str()?, remaining)),
        "authorized-keys" => Some(authorized_keys(remaining)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_canonical_ed25519_public_key_bytes_are_accepted() {
        let key = "AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
        assert!(key_valid(key).is_ok());
        for invalid in [
            String::new(),
            format!("{key}="),
            key.replace('A', "?"),
            key.replacen("AAAAC3", "AAAAA3", 1),
        ] {
            assert!(key_valid(&invalid).is_err());
        }
        assert_eq!(quote("a'b"), "'a'\\''b'");
    }
}
