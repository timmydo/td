//! Durable, private review traces, independent of disposable workspaces.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Instant;

use td_json::Json;

pub(crate) struct Journal {
    file: File,
    directory: std::path::PathBuf,
    sequence: u64,
    session: String,
    started: Instant,
}

pub(crate) fn combine(
    primary: Result<(), String>,
    secondary: Result<(), String>,
) -> Result<(), String> {
    match (primary, secondary) {
        (Err(first), Err(second)) => Err(format!("{first}; {second}")),
        (Err(why), _) | (_, Err(why)) => Err(why),
        (Ok(()), Ok(())) => Ok(()),
    }
}

impl Journal {
    pub(crate) fn create(
        options: &crate::review::Options,
        model: &str,
        key_path: &std::path::Path,
    ) -> Result<Self, String> {
        let state = if options.log_dir.is_none() {
            Some(crate::store::StateDir::from_env(
                std::env::var_os("XDG_STATE_HOME"),
                std::env::var_os("HOME"),
            )?)
        } else {
            None
        };
        let directory = match &options.log_dir {
            Some(path) if path.is_absolute() => path.clone(),
            Some(path) => std::env::current_dir()
                .map_err(|e| format!("review current directory: {e}"))?
                .join(path),
            None => state
                .as_ref()
                .ok_or("review log has no state directory")?
                .root()
                .join("reviews"),
        };
        let mut ancestor = directory.as_path();
        loop {
            match std::fs::symlink_metadata(ancestor) {
                Ok(meta) => {
                    if ancestor == directory && meta.file_type().is_symlink() {
                        return Err(
                            "review log directory must be a private directory, not a symlink"
                                .into(),
                        );
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ancestor = ancestor
                        .parent()
                        .ok_or("log directory has no existing ancestor")?;
                }
                Err(e) => return Err(format!("review log ancestor {}: {e}", ancestor.display())),
            }
        }
        let resolved = std::fs::canonicalize(ancestor)
            .map_err(|e| format!("review log ancestor {}: {e}", ancestor.display()))?;
        let suffix = directory
            .strip_prefix(ancestor)
            .map_err(|e| format!("review log suffix: {e}"))?;
        let mut candidate = resolved.clone();
        for component in suffix.components() {
            match component {
                std::path::Component::Normal(name) => candidate.push(name),
                std::path::Component::ParentDir => {
                    candidate.pop();
                }
                std::path::Component::CurDir => {}
                _ => return Err("invalid review log suffix".into()),
            }
        }
        let ephemeral = std::fs::canonicalize(key_path.parent().ok_or("key path has no parent")?)
            .map_err(|e| format!("review key directory: {e}"))?
            .join("reviews");
        let repository = options
            .repository
            .as_ref()
            .map(|path| {
                std::fs::canonicalize(path)
                    .map_err(|e| format!("review repository {}: {e}", path.display()))
            })
            .transpose()?;
        let mut excluded = vec![ephemeral, std::path::PathBuf::from("/etc")];
        // The jail binds selected /etc entries, including directory aliases.
        // Excluding all resolved entries is a conservative superset.
        match std::fs::read_dir("/etc") {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("review system entries: {e}"))?;
                    match std::fs::canonicalize(entry.path()) {
                        Ok(path) => excluded.push(path),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(format!(
                                "review system entry {}: {e}",
                                entry.path().display()
                            ))
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("review system entries: {e}")),
        }
        excluded.extend(
            crate::jail::SYSTEM_TREES
                .iter()
                .map(std::path::PathBuf::from),
        );
        if let Some(repository) = &repository {
            excluded.push(repository.clone());
        }
        for tree in &excluded {
            if resolved.starts_with(tree) || candidate.starts_with(tree) {
                return Err(format!(
                    "review logs must be outside source, system and disposable trees: {}",
                    tree.display()
                ));
            }
        }
        let state_root = if state.is_some() {
            candidate.parent()
        } else {
            None
        };
        if let Some(state_root) = state_root {
            if state_root.exists() {
                td_fs::check_private_dir(state_root).map_err(|e| e.to_string())?;
            }
        }
        td_fs::private_dir(&candidate).map_err(|e| format!("review log directory: {e}"))?;
        if let Some(state_root) = state_root {
            td_fs::check_private_dir(state_root).map_err(|e| e.to_string())?;
        }
        let directory = std::fs::canonicalize(&candidate)
            .map_err(|e| format!("review log directory {}: {e}", candidate.display()))?;
        if excluded.iter().any(|tree| directory.starts_with(tree)) {
            return Err("review logs must be outside source, system and disposable trees".into());
        }
        let binary = std::env::current_exe().map_err(|e| format!("review agent path: {e}"))?;
        let binary_hash = crate::sha256::sha256_file(std::path::Path::new("/proc/self/exe"))
            .map_err(|e| format!("review agent hash: {e}"))?;
        let session = crate::store::random_hex(16)?;
        let path = directory.join(format!("{session}.jsonl"));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(crate::store::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("creating review log {}: {e}", path.display()))?;
        eprintln!("td-agent review: session log {}", path.display());
        File::open(&directory)
            .and_then(|parent| parent.sync_all())
            .map_err(|e| format!("syncing review log directory: {e}"))?;
        let mut journal = Self {
            file,
            directory,
            sequence: 0,
            session,
            started: Instant::now(),
        };
        journal.event(
            "start",
            Json::Obj(vec![
                ("version".into(), Json::from(1u64)),
                ("session_id".into(), Json::Str(journal.session.clone())),
                (
                    "routing".into(),
                    Json::Str(options.routing.as_deref().unwrap_or("balanced").into()),
                ),
                (
                    "cwd".into(),
                    std::env::current_dir()
                        .ok()
                        .map_or(Json::Null, |path| Json::Str(path.display().to_string())),
                ),
                (
                    "input".into(),
                    options.input.as_ref().map_or(
                        Json::Str(
                            if options.repository.is_some() {
                                "repository"
                            } else {
                                "stdin"
                            }
                            .into(),
                        ),
                        |p| Json::Str(p.display().to_string()),
                    ),
                ),
                (
                    "agent_binary_path".into(),
                    Json::Str(binary.display().to_string()),
                ),
                ("agent_binary_sha256".into(), Json::Str(binary_hash)),
                (
                    "agent_version".into(),
                    Json::Str(env!("CARGO_PKG_VERSION").into()),
                ),
                (
                    "repository".into(),
                    repository
                        .as_ref()
                        .map_or(Json::Null, |p| Json::Str(p.display().to_string())),
                ),
                (
                    "revision".into(),
                    if options.repository.is_some() {
                        Json::Str(options.revision.as_deref().unwrap_or("HEAD").into())
                    } else {
                        Json::Null
                    },
                ),
                ("model".into(), Json::Str(model.into())),
                (
                    "provider_only".into(),
                    Json::Arr(options.providers.iter().cloned().map(Json::Str).collect()),
                ),
                (
                    "provider_fallbacks".into(),
                    Json::Bool(!options.no_provider_fallbacks),
                ),
                (
                    "max_input_price_per_token".into(),
                    options.max_input_price.map_or(Json::Null, Json::from),
                ),
                (
                    "max_output_price_per_token".into(),
                    options.max_output_price.map_or(Json::Null, Json::from),
                ),
            ]),
        )?;
        Ok(journal)
    }

    pub(crate) fn session(&self) -> &str {
        &self.session
    }

    pub(crate) fn outside(&self, paths: &[std::path::PathBuf]) -> Result<(), String> {
        for path in paths {
            let path = match std::fs::canonicalize(path) {
                Ok(path) => path,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => path.clone(),
                Err(e) => return Err(e.to_string()),
            };
            if self.directory.starts_with(&path) {
                return Err(format!(
                    "review logs must be outside jail-visible tree {}",
                    path.display()
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn event(&mut self, kind: &str, data: Json) -> Result<(), String> {
        let line = Json::Obj(vec![
            ("sequence".into(), Json::from(self.sequence)),
            ("time_ms".into(), Json::from(crate::store::now_ms())),
            (
                "elapsed_ms".into(),
                Json::from(self.started.elapsed().as_millis().min(u64::MAX as u128) as u64),
            ),
            ("kind".into(), Json::Str(kind.into())),
            ("data".into(), data),
        ])
        .to_string();
        self.file
            .write_all(line.as_bytes())
            .and_then(|()| self.file.write_all(b"\n"))
            .and_then(|()| self.file.flush())
            .map_err(|e| format!("writing review session log: {e}"))?;
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }

    pub(crate) fn text(&mut self, kind: &str, text: &str) -> Result<(), String> {
        self.event(kind, Json::Str(text.into()))
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.raw("response_bytes_hex", bytes)
    }

    pub(crate) fn raw(&mut self, kind: &str, bytes: &[u8]) -> Result<(), String> {
        self.text(kind, &crate::host::encode_hex(bytes))
    }

    pub(crate) fn model(&mut self, model: &crate::models::Model) -> Result<(), String> {
        let metadata = crate::models::Models {
            models: vec![model.clone()],
        }
        .to_cache();
        self.event(
            "model_metadata",
            td_json::parse(&metadata).map_err(|e| e.to_string())?,
        )
    }

    pub(crate) fn outcome(
        &mut self,
        kind: &str,
        result: &Result<(), String>,
    ) -> Result<(), String> {
        self.event(
            kind,
            Json::Obj(vec![
                ("success".into(), Json::Bool(result.is_ok())),
                (
                    "error".into(),
                    result.as_ref().err().cloned().map_or(Json::Null, Json::Str),
                ),
            ]),
        )
    }

    pub(crate) fn end(&mut self, result: &Result<(), String>) -> Result<(), String> {
        self.outcome("end", result)?;
        self.file
            .sync_all()
            .map_err(|e| format!("syncing review log: {e}"))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn logging_failure_preserves_the_original_failure() {
        let error = combine(Err("model stream broke".into()), Err("disk full".into())).unwrap_err();
        assert!(error.contains("model stream broke"));
        assert!(error.contains("disk full"));
    }

    #[test]
    fn trace_preserves_bytes_and_refuses_shared_or_linked_directories() {
        let root = std::env::temp_dir().join(format!(
            "td-review-log-{}",
            crate::store::random_hex(8).unwrap()
        ));
        let options = crate::review::Options {
            log_dir: Some(root.join("private")),
            ..Default::default()
        };
        td_fs::private_dir(&root).unwrap();
        let key_path = root.join("openrouter.key");
        let mut journal = Journal::create(&options, "test/model", &key_path).unwrap();
        journal.bytes(&[0, 255, 195, 169]).unwrap();
        journal.end(&Err("interrupted test".into())).unwrap();
        let path = std::fs::read_dir(options.log_dir.as_ref().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("00ffc3a9"));
        for line in text.lines() {
            assert!(td_json::parse(line).is_ok());
        }
        std::fs::set_permissions(
            options.log_dir.as_ref().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(Journal::create(&options, "test/model", &key_path)
            .err()
            .unwrap()
            .contains("private directory"));
        std::fs::set_permissions(
            options.log_dir.as_ref().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(options.log_dir.as_ref().unwrap(), &link).unwrap();
        assert!(Journal::create(
            &crate::review::Options {
                log_dir: Some(link),
                ..Default::default()
            },
            "test/model",
            &key_path
        )
        .err()
        .unwrap()
        .contains("not a symlink"));
        let source = root.join("source");
        td_fs::private_dir(&source).unwrap();
        let inside = crate::review::Options {
            repository: Some(source.clone()),
            log_dir: Some(source.join("logs")),
            ..Default::default()
        };
        assert!(Journal::create(&inside, "test/model", &key_path)
            .err()
            .unwrap()
            .contains("outside source"));
        assert!(!source.join("logs").exists());
        let unresolved = crate::review::Options {
            repository: Some(source.clone()),
            log_dir: Some(root.join("missing/../source/logs")),
            ..Default::default()
        };
        assert!(Journal::create(&unresolved, "test/model", &key_path)
            .err()
            .unwrap()
            .contains("outside source"));
        assert!(!root.join("missing").exists());
        assert!(!source.join("logs").exists());
        let outside = crate::review::Options {
            repository: Some(source.clone()),
            log_dir: Some(source.join("../outside")),
            ..Default::default()
        };
        Journal::create(&outside, "test/model", &key_path)
            .unwrap()
            .end(&Ok(()))
            .unwrap();
        let ephemeral = crate::review::Options {
            log_dir: Some(root.join("reviews/run-log")),
            ..Default::default()
        };
        assert!(Journal::create(&ephemeral, "test/model", &key_path)
            .err()
            .unwrap()
            .contains("outside source"));
        assert!(!root.join("reviews").exists());
        let system = crate::review::Options {
            log_dir: Some(std::path::PathBuf::from("/usr/td-review-refused")),
            ..Default::default()
        };
        assert!(Journal::create(&system, "test/model", &key_path)
            .err()
            .unwrap()
            .contains("outside source"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
