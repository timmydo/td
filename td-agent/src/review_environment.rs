//! Declared offline review inputs and bounded manifest preflight.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use td_json::Json;
use td_toml::Toml;

fn relative(base: &Path, path: &str) -> Option<PathBuf> {
    let mut result = base.to_path_buf();
    for part in Path::new(path).components() {
        match part {
            Component::Normal(name) => result.push(name),
            Component::CurDir => {}
            Component::ParentDir if result.pop() => {}
            _ => return None,
        }
    }
    Some(result)
}

fn paths(value: &Toml, found: &mut Vec<String>) {
    if let Toml::Table(fields) = value {
        for (key, value) in fields {
            if matches!(
                key.as_str(),
                "dependencies" | "dev-dependencies" | "build-dependencies"
            ) {
                if let Toml::Table(deps) = value {
                    for (_, spec) in deps {
                        if let Some(path) = spec.get("path").and_then(Toml::as_str) {
                            found.push(path.into());
                        }
                    }
                }
            } else {
                paths(value, found);
            }
        }
    }
}

pub(crate) fn prepare(
    workspace: &mut crate::review_workspace::Workspace,
    options: &crate::review::Options,
    key_path: &Path,
    journal: &mut crate::review_log::Journal,
) -> Result<String, String> {
    let mut initial = workspace.sparse.clone();
    initial.insert(0, String::new());
    let mut pending: Vec<PathBuf> = initial
        .iter()
        .map(|p| PathBuf::from(p).join("Cargo.toml"))
        .collect();
    pending.push(PathBuf::from("Cargo.toml"));
    let mut seen = BTreeSet::new();
    let mut limitations = Vec::new();
    while let Some(manifest) = pending.pop() {
        if !seen.insert(manifest.clone()) {
            continue;
        }
        if seen.len() > 128 {
            limitations.push("path-dependency inspection stopped at 128 manifests".into());
            break;
        }
        let text = match workspace.tracked_text(&manifest) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(why) => {
                limitations.push(format!(
                    "{}: manifest inspection unavailable: {why}",
                    manifest.display()
                ));
                continue;
            }
        };
        let parsed = match td_toml::parse(&text) {
            Ok(value) => value,
            Err(e) => {
                limitations.push(format!("{}: {e}", manifest.display()));
                continue;
            }
        };
        let mut dependencies = Vec::new();
        if let Some(members) = parsed
            .get("workspace")
            .and_then(|w| w.get("members"))
            .and_then(Toml::as_arr)
        {
            for member in members {
                if let Some(path) = member.as_str() {
                    if path.contains(['*', '?', '[']) {
                        limitations.push(format!("{}: wildcard workspace member {path} requires explicit sparse selection", manifest.display()));
                    } else {
                        dependencies.push(path.to_string());
                    }
                }
            }
        }
        paths(&parsed, &mut dependencies);
        let base = manifest.parent().unwrap_or(Path::new(""));
        for dependency in dependencies {
            let Some(path) = relative(base, &dependency) else {
                limitations.push(format!(
                    "{} has an outside-repository dependency",
                    manifest.display()
                ));
                continue;
            };
            if let Some(Component::Normal(top)) = path.components().next() {
                let top = top.to_str().ok_or("dependency path is not UTF-8")?;
                if !workspace.sparse.iter().any(|p| p == top) {
                    if let Err(why) = workspace.expand(&[top.to_string()]) {
                        limitations.push(format!(
                            "{}: path dependency {dependency} unavailable: {why}",
                            manifest.display()
                        ));
                        continue;
                    }
                }
            }
            pending.push(path.join("Cargo.toml"));
        }
    }
    let home = workspace.scratch.join("cargo-home");
    std::fs::create_dir(&home).map_err(|e| format!("review Cargo home: {e}"))?;
    let mut config = String::new();
    if let Some(vendor) = &options.vendor {
        let vendor = std::fs::canonicalize(vendor).map_err(|e| format!("review vendor: {e}"))?;
        let key = std::fs::canonicalize(key_path).map_err(|e| e.to_string())?;
        if key.starts_with(&vendor) || vendor.starts_with(key.parent().ok_or("key has no parent")?)
        {
            return Err("review vendor must not expose the credential directory".into());
        }
        if !vendor.is_dir() {
            return Err("review vendor is not a directory".into());
        }
        let quoted = Json::Str(vendor.display().to_string()).to_string();
        config = format!("[source.crates-io]\nreplace-with = \"review-vendor\"\n[source.review-vendor]\ndirectory = {quoted}\n");
        workspace.vendor = Some(vendor);
    }
    let automatic = PathBuf::from(
        options
            .repository
            .as_ref()
            .ok_or("review has no repository")?,
    )
    .join("target/release/td-builder");
    let runner = options
        .test_runner
        .as_ref()
        .or_else(|| automatic.is_file().then_some(&automatic));
    if let Some(runner) = runner {
        match stage_runner(workspace, runner) {
            Ok(hash) => journal.event(
                "test_runner",
                Json::Obj(vec![
                    ("source".into(), Json::Str(runner.display().to_string())),
                    ("sha256".into(), Json::Str(hash)),
                ]),
            )?,
            Err(why) if options.test_runner.is_none() => {
                limitations.push(format!("Automatic test runner unavailable: {why}"))
            }
            Err(why) => return Err(why),
        }
    } else {
        limitations.push("No td-builder test runner was supplied or found at source target/release/td-builder; a configured repository runner may be unavailable.".into());
    }
    std::fs::write(home.join("config.toml"), config).map_err(|e| e.to_string())?;
    workspace.cargo_home = Some(home);
    workspace.log_environment(journal)?;
    journal.event(
        "prepared_environment",
        Json::Obj(vec![
            (
                "sparse".into(),
                Json::Arr(workspace.sparse.iter().cloned().map(Json::Str).collect()),
            ),
            (
                "vendor".into(),
                workspace
                    .vendor
                    .as_ref()
                    .map_or(Json::Null, |p| Json::Str(p.display().to_string())),
            ),
            (
                "runner".into(),
                workspace.runner.clone().map_or(Json::Null, Json::Str),
            ),
            (
                "cargo_home".into(),
                workspace
                    .cargo_home
                    .as_ref()
                    .map_or(Json::Null, |p| Json::Str(p.display().to_string())),
            ),
        ]),
    )?;
    if initial.len() > 16 {
        limitations.push("metadata preflight limited to the first 16 selected directories".into());
    }
    for directory in initial.iter().take(16) {
        let manifest = PathBuf::from(directory).join("Cargo.toml");
        match workspace.tracked_text(&manifest) {
            Ok(Some(_)) => {}
            Ok(None) => continue,
            Err(why) => {
                limitations.push(format!(
                    "{}: metadata preflight unavailable: {why}",
                    manifest.display()
                ));
                continue;
            }
        }
        // Metadata resolves locked dependencies but never executes build scripts.
        let path = workspace.checkout.join(&manifest);
        let command = format!(
            "cargo metadata --offline --locked --format-version 1 --manifest-path {}",
            shell_quote(&path.display().to_string())
        );
        let started = std::time::Instant::now();
        let result = workspace.call_logged(
            "shell",
            &Json::Obj(vec![
                ("command".into(), Json::Str(command)),
                ("timeout_ms".into(), Json::from(15_000u64)),
            ])
            .to_string(),
            |kind, bytes| {
                if kind == "tool_output_bytes_hex" {
                    journal.raw("preflight_output_bytes_hex", bytes)
                } else {
                    journal.text(
                        "preflight_output",
                        std::str::from_utf8(bytes).map_err(|e| e.to_string())?,
                    )
                }
            },
        );
        let text = result.clone().unwrap_or_else(|why| why);
        let available = text.starts_with("[exit status 0]");
        journal.event(
            "test_preflight",
            Json::Obj(vec![
                ("manifest".into(), Json::Str(manifest.display().to_string())),
                ("available".into(), Json::Bool(available)),
                (
                    "duration_ms".into(),
                    Json::from(started.elapsed().as_millis().min(u64::MAX as u128) as u64),
                ),
                ("result".into(), Json::Str(text.clone())),
            ]),
        )?;
        if !available {
            limitations.push(format!(
                "{}: {}",
                manifest.display(),
                text.chars().take(1500).collect::<String>()
            ));
        }
    }
    let summary = format!("Offline test preparation: {}. Path dependencies expanded within this commit; private Cargo home {}; vendor {}; runner {}. Metadata preflight checks inputs, not tests.", if limitations.is_empty() { "selected inputs resolved".into() } else { limitations.join("\n") }, workspace.cargo_home.as_ref().map_or_else(|| "none".into(), |p| p.display().to_string()), workspace.vendor.as_ref().map_or_else(|| "none supplied".into(), |p| p.display().to_string()), workspace.runner.as_deref().unwrap_or("none supplied"));
    journal.text("test_environment", &summary)?;
    Ok(summary.chars().take(8192).collect())
}

fn stage_runner(
    workspace: &mut crate::review_workspace::Workspace,
    runner: &Path,
) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(runner).map_err(|e| format!("review runner: {e}"))?;
    if !meta.is_file() || meta.mode() & 0o111 == 0 || meta.len() > 64 * 1024 * 1024 {
        return Err("review runner must be a direct executable file of at most 64 MiB".into());
    }
    let mut input = File::options()
        .read(true)
        .custom_flags(crate::store::O_NOFOLLOW)
        .open(runner)
        .map_err(|e| e.to_string())?;
    let mut magic = [0u8; 4];
    input.read_exact(&mut magic).map_err(|e| e.to_string())?;
    if magic != *b"\x7fELF" {
        return Err("review runner must be an ELF td-builder binary".into());
    }
    use std::io::Seek;
    input.rewind().map_err(|e| e.to_string())?;
    let target = workspace.scratch.join("review-test-runner");
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .custom_flags(crate::store::O_NOFOLLOW)
        .open(&target)
        .map_err(|e| e.to_string())?;
    let count = std::io::copy(&mut input.take(64 * 1024 * 1024 + 1), &mut output)
        .map_err(|e| e.to_string())?;
    if count > 64 * 1024 * 1024 {
        return Err("review runner grew past its bound".into());
    }
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    let text = target.to_str().ok_or("review runner path is not UTF-8")?;
    if text.chars().any(char::is_whitespace) {
        return Err("review runner path must not contain whitespace".into());
    }
    let hash = crate::sha256::sha256_file(&target).map_err(|e| e.to_string())?;
    workspace.runner = Some(text.into());
    Ok(hash)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dependency_paths_cannot_leave_the_repository() {
        assert_eq!(
            relative(Path::new("td-json"), "../td-header"),
            Some(PathBuf::from("td-header"))
        );
        assert!(relative(Path::new("td-json"), "../../secret").is_none());
        assert!(relative(Path::new(""), "/home/secret").is_none());
    }
}
