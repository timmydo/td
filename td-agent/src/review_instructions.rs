//! Full, commit-pinned repository guidance retained with the original diff.
use std::path::Path;
use td_json::Json;

const MAX_TOTAL: usize = 1024 * 1024;
const MAX_FILES: usize = 128;

fn applicable(path: &Path, changed: &[String], sparse: &[String]) -> bool {
    let directory = path.parent().unwrap_or(Path::new(""));
    changed.iter().any(|p| {
        Path::new(p)
            .parent()
            .is_some_and(|parent| parent.starts_with(directory))
    }) || sparse.iter().any(|p| Path::new(p).starts_with(directory))
        || directory.as_os_str().is_empty()
}

pub(crate) fn seed(
    workspace: &crate::review_workspace::Workspace,
    journal: &mut crate::review_log::Journal,
) -> Result<String, String> {
    let mut records = Vec::new();
    let mut total = 0usize;
    for path in workspace.guidance_paths()? {
        if !applicable(&path, &workspace.changed, &workspace.sparse) {
            continue;
        }
        if records.len() == MAX_FILES {
            return Err("too many applicable AGENTS.md files".into());
        }
        let Some(text) = workspace.tracked_guidance(&path)? else {
            continue;
        };
        total = total.saturating_add(text.len());
        if total > MAX_TOTAL {
            return Err(
                "full applicable AGENTS.md instructions exceed 1 MiB; no review requested".into(),
            );
        }
        let hash = crate::sha256::sha256_reader(text.as_bytes()).map_err(|e| e.to_string())?;
        records.push(Json::Obj(vec![
            ("path".into(), Json::Str(path.display().to_string())),
            ("sha256".into(), Json::Str(hash)),
            ("text".into(), Json::Str(text)),
        ]));
    }
    let data = Json::Obj(vec![
        ("commit".into(), Json::Str(workspace.commit.clone())),
        ("files".into(), Json::Arr(records)),
        ("complete".into(), Json::Bool(true)),
    ]);
    journal.event("review_instructions", data.clone())?;
    Ok(data.to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn broad_changes_match_existing_guidance_without_counting_absent_candidates() {
        let changed = (0..128)
            .map(|n| format!("crate{n}/src/nested/file.rs"))
            .collect::<Vec<_>>();
        assert!(applicable(Path::new("AGENTS.md"), &changed, &[]));
        assert!(applicable(
            Path::new("crate127/src/AGENTS.md"),
            &changed,
            &[]
        ));
        assert!(!applicable(Path::new("crate128/AGENTS.md"), &changed, &[]));
        assert!(!applicable(
            Path::new("crate127/src/nested/file.rs/AGENTS.md"),
            &changed,
            &[]
        ));
        assert!(applicable(
            Path::new("dep/AGENTS.md"),
            &[],
            &["dep/src".into()]
        ));
    }
}
