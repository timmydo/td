//! Physical selection of authenticated retained deployments.
use super::*;
use std::io::Read;

struct Entry {
    id: String,
    slot: &'static str,
    label: String,
    attempts: Option<u8>,
}

fn entries(root: &Path, key: &TrustRoot) -> io::Result<Vec<Entry>> {
    let mut entries: Vec<Entry> = Vec::new();
    for slot in ["current", "previous"] {
        // Labels need only the signed manifest and its hashed initramfs.
        // Verify the full kernel/root payloads only on the selected boot path.
        let candidate = (|| {
            let id = read_selector(root, slot)?;
            let (directory, manifest) = authenticated_manifest(root, &id, key)?;
            let initramfs = verify_payload(&directory, "initramfs.cpio", &manifest.initramfs)?;
            Ok::<_, io::Error>((id, initramfs))
        })();
        let (id, mut initramfs) = match candidate {
            Ok(value) => value,
            Err(error) => {
                writeln!(
                    mirror::stderr(),
                    "td-boot: menu {slot} unavailable: {error}"
                )?;
                continue;
            }
        };
        if entries.iter().any(|entry| entry.id == id) {
            continue;
        }
        let attempts = match read_attempt_state(root, &id) {
            Ok(value) => value,
            Err(error) => {
                writeln!(
                    mirror::stderr(),
                    "td-boot: menu {slot} bookkeeping unavailable: {error}"
                )?;
                continue;
            }
        };
        // Retrying a pending previous requires a known-good current fallback.
        if slot == "previous"
            && attempts.is_some()
            && !entries
                .iter()
                .any(|entry| entry.slot == "current" && entry.attempts.is_none())
        {
            continue;
        }
        let info = boot_info::read(&mut initramfs)?;
        let identity = match info {
            Some(info) => info.label().map_err(invalid)?,
            None => format!(
                "commit date/hash unknown  build date unknown  id {}",
                id.get(..12)
                    .ok_or_else(|| invalid("invalid deployment ID"))?
            ),
        };
        let state = match attempts {
            None => "ready".into(),
            Some(0) => "failed; retry".into(),
            Some(n) => format!("{n} attempts left"),
        };
        entries.push(Entry {
            id,
            slot,
            label: format!("{slot}: {identity} [{state}]"),
            attempts,
        });
    }
    Ok(entries)
}

fn default_index(entries: &[Entry]) -> usize {
    if let Some(index) = entries
        .iter()
        .position(|entry| entry.slot == "current" && entry.attempts != Some(0))
    {
        return index;
    }
    entries
        .iter()
        .position(|entry| entry.slot == "previous" && entry.attempts.is_none())
        .unwrap_or(0)
}

/// The transaction lock remains held across the prompt and selection.
/// A timeout preserves the ordinary automatic rollback policy.
pub(super) fn choose(root: &Path, key: &TrustRoot) -> io::Result<Option<String>> {
    let entries = entries(root, key)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut child = Command::new(format!("/bin/{}", protocol::BOOT_MENU_APPLET))
        .arg(default_index(&entries).to_string())
        .args(entries.iter().map(|entry| entry.label.as_str()))
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let result = (|| {
        let mut reply = Vec::new();
        child
            .stdout
            .take()
            .ok_or_else(|| invalid("boot menu has no output"))?
            .take(16)
            .read_to_end(&mut reply)?;
        match reply.as_slice() {
            b"auto\n" => Ok(None),
            b"0\n" => entries
                .first()
                .map(|entry| Some(entry.id.clone()))
                .ok_or_else(|| invalid("missing first boot entry")),
            b"1\n" => entries
                .get(1)
                .map(|entry| Some(entry.id.clone()))
                .ok_or_else(|| invalid("missing second boot entry")),
            _ => Err(invalid("invalid boot menu result")),
        }
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    if !status.success() {
        return Err(invalid(format!("boot menu failed: {status}")));
    }
    result
}

/// A physical choice makes that system current so older deployments' success
/// acknowledgement still works. Publication preserves a known-good fallback.
pub(super) fn select(root: &Path, key: &TrustRoot, id: &str) -> io::Result<BootDecision> {
    let current = authenticated_slot(root, "current", key);
    if read_selector(root, "current").is_ok_and(|named| named == id) {
        let current = current?;
        if current.id != id {
            return Err(invalid("current changed during boot selection"));
        }
        if read_attempt_state(root, id)?.is_some() {
            write_attempt_state(root, id, protocol::DEFAULT_BOOT_ATTEMPTS)?;
        }
    } else {
        let current = current.ok();
        let previous = authenticated_slot(root, "previous", key)?;
        if previous.id != id {
            return Err(invalid("the chosen deployment is no longer retained"));
        }
        if read_attempt_state(root, id)?.is_none() {
            replace_selector(root, "current", id)?;
            if let Some(current) = current {
                replace_selector(root, "previous", &current.id)?;
            }
        } else {
            let current =
                current.ok_or_else(|| invalid("retry requires a verified current fallback"))?;
            if read_attempt_state(root, &current.id)?.is_some() {
                return Err(invalid("retry requires a successful current fallback"));
            }
            activate_install(root, &current.id, id)?;
        }
    }
    let remaining_attempts = match consume_boot_attempt(root, id)? {
        AttemptDecision::Successful => None,
        AttemptDecision::Consumed(remaining) => Some(remaining),
        AttemptDecision::Exhausted => {
            return Err(invalid("chosen deployment has no boot attempts remaining"))
        }
    };
    Ok(BootDecision {
        slot: "current",
        deployment_id: id.to_string(),
        current_error: None,
        exhausted_deployment: None,
        fallback_error: None,
        bookkeeping_error: None,
        remaining_attempts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(slot: &'static str, attempts: Option<u8>) -> Entry {
        Entry {
            id: String::new(),
            slot,
            label: String::new(),
            attempts,
        }
    }
    #[test]
    fn timeout_default_preserves_attempt_budget_and_rollback_policy() {
        assert_eq!(
            default_index(&[entry("current", None), entry("previous", None)]),
            0
        );
        assert_eq!(
            default_index(&[entry("current", Some(1)), entry("previous", None)]),
            0
        );
        assert_eq!(
            default_index(&[entry("current", Some(0)), entry("previous", None)]),
            1
        );
        assert_eq!(default_index(&[entry("previous", None)]), 0);
        assert_eq!(default_index(&[entry("current", Some(0))]), 0);
    }
}
