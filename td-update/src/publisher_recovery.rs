//! Explicit local consent changes future publisher trust, never an SSH update.

use super::*;
#[path = "../../td-authd/src/primary_account.rs"]
#[allow(dead_code, reason = "shared validated primary account lookup")]
mod primary_account;

const STAGING: &str = ".releases-recovery";
const JOURNAL: &str = "release-recovery";

fn digest(value: &str) -> Result<()> {
    if !crate::protocol::valid_digest(value.as_bytes()) {
        return Err("publisher fingerprint must be 64 lowercase hexadecimal digits".into());
    }
    Ok(())
}

fn candidate(home: &Path, expected: &str) -> PathBuf {
    home.join(".td-update/publishers").join(expected)
}

fn owned_root(path: &Path, expected: &str, owner: u32) -> Result<String> {
    digest(expected)?;
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(path),
        "open requested publisher root",
    )?;
    let meta = io(file.metadata(), "inspect requested publisher root")?;
    if !meta.is_file() || meta.uid() != owner || meta.len() == 0 || meta.len() > 16384 {
        return Err("publisher root must be a bounded regular file owned by the requester".into());
    }
    let mut bytes = Vec::new();
    io(
        file.take(16385).read_to_end(&mut bytes),
        "read requested publisher root",
    )?;
    if bytes.len() > 16384 || crate::sha256::hex_digest(&bytes) != expected {
        return Err("publisher root does not match the explicitly supplied fingerprint".into());
    }
    String::from_utf8(bytes).map_err(|_| "publisher root is not UTF-8".into())
}

pub(super) fn request(source: &Path, expected: &str) -> Result<()> {
    if io(fs::metadata("/proc/self"), "read requester identity")?.uid() != 1000 {
        return Err("publisher trust must be requested by the primary account".into());
    }
    let home = io(primary_account::load(), "read primary account")?.persistent_home();
    let text = owned_root(source, expected, 1000)?;
    for directory in [home.join(".td-update"), home.join(".td-update/publishers")] {
        crate::td_fs::private_dir(&directory).map_err(|e| e.to_string())?;
    }
    // Content identity, not this pathname, is what the authority approves.
    publish(&candidate(&home, expected), &text)?;
    crate::success(
        fixed("/bin/td-authd").args(["request-hostname", &format!("publisher {expected}")]),
        "request publisher replacement; approve H on secure attention",
    )
}

fn remove_staging(state: &Path) -> Result<()> {
    let staging = state.join(STAGING);
    if crate::present(&staging)? {
        let meta = io(fs::symlink_metadata(&staging), "inspect publisher staging")?;
        if !meta.is_dir() || meta.uid() != io(state.metadata(), "inspect publisher owner")?.uid() {
            return Err("publisher staging is not an owned directory".into());
        }
        io(
            fs::remove_dir_all(staging),
            "remove unpublished publisher staging",
        )?;
    }
    Ok(())
}

pub(super) fn accept(expected: &str) -> Result<()> {
    root()?;
    digest(expected)?;
    directory(Path::new(STATE), 0, 0, 0o755)?;
    let state = Path::new(STATE);
    let lease = enrollment_lease(state)?;
    // A dead service may have left helpers alive after its lease was released.
    drain_group(GIT_GROUP)?;
    drain_group(OPERATION_GROUP)?;
    drain_group(BUILD_GROUP)?;
    finish(state)?;
    let home = io(primary_account::load(), "read primary account")?.persistent_home();
    let text = owned_root(&candidate(&home, expected), expected, 1000)?;
    let endpoint = read(&state.join("releases/endpoint"), 4097)?;
    let endpoint = endpoint
        .strip_suffix('\n')
        .ok_or("unterminated publisher endpoint")?;
    if endpoint.is_empty() || endpoint.len() > 4096 || endpoint.contains(['\r', '\n', '\0']) {
        return Err("invalid saved publisher endpoint".into());
    }
    remove_staging(state)?;
    let staging = state.join(STAGING);
    io(
        fs::DirBuilder::new().mode(0o700).create(&staging),
        "create publisher staging",
    )?;
    publish(&staging.join("root"), &text)?;
    // The release parser preserves the profile but starts a new trust history.
    crate::success(
        fixed("/bin/td-release")
            .arg("reanchor")
            .arg(state.join("releases/state"))
            .arg(staging.join("root"))
            .arg(staging.join("state"))
            .stdin(Stdio::from(io(
                lease.0.try_clone(),
                "retain publisher validation lease",
            )?)),
        "validate replacement publisher policy",
    )?;
    publish(&staging.join("endpoint"), &format!("{endpoint}\n"))?;
    let mut nonce = [0; 32];
    io(
        File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut nonce)),
        "draw policy history identity",
    )?;
    publish(
        &state.join(JOURNAL),
        &format!("{expected}\n{}\n", crate::sha256::hex_digest(&nonce)),
    )?;
    finish(state)
}

fn sync(state: &Path) -> Result<()> {
    io(
        File::open(state).and_then(|file| file.sync_all()),
        "persist publisher transaction",
    )
}

/// A durable consent record precedes either rename; boot never substitutes the
/// bundled default in the gap between archiving old trust and publishing new.
pub(super) fn finish(state: &Path) -> Result<()> {
    finish_with_worker(state, Path::new(BUILD_HOME))
}

fn finish_with_worker(state: &Path, worker: &Path) -> Result<()> {
    let journal = state.join(JOURNAL);
    if !crate::present(&journal)? {
        return Ok(());
    }
    let record = read(&journal, 130)?;
    let mut lines = record.split('\n');
    let expected = lines.next().ok_or("missing publisher fingerprint")?;
    let nonce = lines.next().ok_or("missing publisher history identity")?;
    if lines.next() != Some("") || lines.next().is_some() {
        return Err("invalid publisher recovery record".into());
    }
    digest(expected)?;
    digest(nonce)?;
    let staging = state.join(STAGING);
    let destination = state.join("releases");
    let backup = state.join(format!("releases-before-{nonce}"));
    if crate::present(&staging)? {
        if crate::sha256::hex_digest(read(&staging.join("root"), 16384)?.as_bytes()) != expected {
            return Err("staged publisher differs from the approved fingerprint".into());
        }
        if crate::present(&destination)? {
            if crate::present(&backup)? {
                return Err("publisher history already exists; refusing ambiguous recovery".into());
            }
            io(
                fs::rename(&destination, &backup),
                "retain previous publisher policy",
            )?;
            sync(state)?;
        } else if !crate::present(&backup)? {
            return Err("publisher recovery has neither current nor previous policy".into());
        }
        io(
            fs::rename(&staging, &destination),
            "activate approved publisher policy",
        )?;
        sync(state)?;
    }
    if crate::sha256::hex_digest(read(&destination.join("root"), 16384)?.as_bytes()) != expected {
        return Err("active publisher differs from the approved fingerprint".into());
    }
    // Prior evidence and immutable root versions belong to the retired trust
    // history. Keep the journal until their removal is durable and retryable.
    if crate::present(worker)? {
        if !io(fs::symlink_metadata(worker), "inspect downloader home")?.is_dir() {
            return Err("downloader home is not a directory".into());
        }
        for name in ["release-state.roots", "release-state.metadata"] {
            let path = worker.join(name);
            if crate::present(&path)? {
                let meta = io(
                    fs::symlink_metadata(&path),
                    "inspect old publisher evidence",
                )?;
                io(
                    if meta.is_dir() {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    },
                    "remove old publisher evidence",
                )?;
            }
        }
        sync(worker)?;
    }
    io(fs::remove_file(journal), "finish publisher recovery record")?;
    sync(state)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn recovery_finishes_each_rename_boundary_without_default_trust_or_lost_history() {
        for boundary in 0..=3 {
            let scratch = crate::temp_dir::TempDir::new("publisher-recovery").unwrap();
            let state = scratch.path();
            let worker = state.join("worker");
            fs::create_dir(&worker).unwrap();
            for name in ["release-state.roots", "release-state.metadata"] {
                fs::create_dir(worker.join(name)).unwrap();
                fs::write(worker.join(name).join("1"), b"old publisher root").unwrap();
            }
            fs::write(worker.join("release-cache"), b"keep verified chunks").unwrap();
            let old = state.join("releases");
            let staging = state.join(STAGING);
            let nonce = "b".repeat(64);
            let backup = state.join(format!("releases-before-{nonce}"));
            let expected = crate::sha256::hex_digest(b"approved root\n");
            fs::create_dir(&old).unwrap();
            fs::write(old.join("root"), b"old trust and counters\n").unwrap();
            fs::create_dir(&staging).unwrap();
            fs::write(staging.join("root"), b"approved root\n").unwrap();
            fs::write(staging.join("state"), b"new verified state\n").unwrap();
            publish(&state.join(JOURNAL), &format!("{expected}\n{nonce}\n")).unwrap();
            if boundary >= 1 {
                fs::rename(&old, &backup).unwrap();
            }
            if boundary >= 2 {
                fs::rename(&staging, &old).unwrap();
            }
            if boundary == 3 {
                fs::remove_dir_all(worker.join("release-state.roots")).unwrap();
            }
            finish_with_worker(state, &worker).unwrap();
            assert_eq!(fs::read(old.join("root")).unwrap(), b"approved root\n");
            assert_eq!(
                fs::read(backup.join("root")).unwrap(),
                b"old trust and counters\n"
            );
            assert!(!state.join(JOURNAL).exists());
            assert!(!worker.join("release-state.roots").exists());
            assert!(!worker.join("release-state.metadata").exists());
            assert_eq!(
                fs::read(worker.join("release-cache")).unwrap(),
                b"keep verified chunks"
            );
            finish_with_worker(state, &worker).unwrap();
        }
    }

    #[test]
    fn altered_candidate_and_malformed_journal_cannot_replace_policy() {
        let scratch = crate::temp_dir::TempDir::new("publisher-binding").unwrap();
        let state = scratch.path();
        fs::create_dir(state.join("releases")).unwrap();
        fs::write(state.join("releases/root"), b"old").unwrap();
        fs::create_dir(state.join(STAGING)).unwrap();
        fs::write(state.join(STAGING).join("root"), b"substitution").unwrap();
        for record in [
            "malformed\n".to_string(),
            format!("{}\n{}\n", "a".repeat(64), "b".repeat(64)),
        ] {
            fs::write(state.join(JOURNAL), record).unwrap();
            assert!(finish_with_worker(state, &state.join("worker")).is_err());
            assert_eq!(fs::read(state.join("releases/root")).unwrap(), b"old");
        }
        let path = state.join("candidate");
        fs::write(&path, b"approved").unwrap();
        let expected = crate::sha256::hex_digest(b"approved");
        let owner = fs::metadata(&path).unwrap().uid();
        assert_eq!(owned_root(&path, &expected, owner).unwrap(), "approved");
        assert!(owned_root(&path, &expected, owner + 1).is_err());
        fs::write(&path, b"substituted").unwrap();
        assert!(owned_root(&path, &expected, owner).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink("releases/root", &path).unwrap();
        assert!(owned_root(&path, &crate::sha256::hex_digest(b"old"), owner).is_err());
    }
}
