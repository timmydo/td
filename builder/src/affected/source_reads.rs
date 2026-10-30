//! Conservative file scopes for the first hop of shared-source readers.

use std::path::{Path, PathBuf};
use td_engine::embed_scan::{
    code_embed_literals, embed_literals, has_computed_embed, has_nested_path_attribute,
    has_unresolved_modules, normalize_join, string_literal_span, strip_comments,
};

/// Unknown or assembled paths retain the whole-crate edge.
pub(super) fn may_read_changed(
    root: &Path,
    text: &str,
    donor: &str,
    changed: &[String],
) -> Result<bool, String> {
    if has_computed_embed(&strip_comments(text)) {
        return Ok(true);
    }
    let Some(mut pending) = named_files(text, donor) else {
        return Ok(true);
    };
    let mut visited = Vec::new();
    while let Some(file) = pending.pop() {
        if changed.iter().any(|p| p == &file) {
            return Ok(true);
        }
        if visited.contains(&file) {
            continue;
        }
        if visited.len() >= 256 {
            return Ok(true);
        }
        visited.push(file.clone());
        let path = root.join(&file);
        for ancestor in path.ancestors().take_while(|p| *p != root) {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(e) => {
                    return Err(format!(
                        "{} could not be inspected: {e}",
                        ancestor.display()
                    ))
                }
            }
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(e) => return Err(format!("{} could not be inspected: {e}", path.display())),
        };
        if !metadata.is_file() {
            return Ok(true);
        }
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("{} could not be read: {e}", path.display()))?;
        // A textual include may itself be Rust, whatever its extension.
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let code = strip_comments(&text);
        if has_computed_embed(&code) || has_unresolved_modules(&code) {
            return Ok(true);
        }
        let Some(dir) = file.rsplit_once('/').map(|(dir, _)| dir) else {
            return Ok(true);
        };
        for (_, literal) in embed_literals(&code) {
            if literal.contains('\\') {
                return Ok(true);
            }
            let Some(included) = normalize_join(dir, &literal) else {
                return Ok(true);
            };
            pending.push(included);
        }
        // A shared file can inspect another donor file without an embed.
        let Some(named) = named_files(&code, donor) else {
            return Ok(true);
        };
        pending.extend(named);
    }
    Ok(false)
}

/// All directory mentions must finish as literal file paths. A directory,
/// placeholder, escape or parent traversal cannot establish a narrow scope.
fn named_files(text: &str, donor: &str) -> Option<Vec<String>> {
    let needle = format!("{donor}/");
    let mut files = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(&needle) {
        let before = rest.get(..at)?.chars().next_back();
        let tail = rest.get(at..)?;
        rest = tail.get(needle.len()..)?;
        if before.is_some_and(super::is_name_char) {
            continue;
        }
        let len = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || "-._/".contains(c)))
            .unwrap_or(tail.len());
        let file = tail.get(..len)?;
        let after = tail.get(len..)?;
        if !(after.starts_with('"') || after.starts_with("\\\""))
            || file.split('/').any(|c| matches!(c, "" | "." | ".."))
            || !file.rsplit('/').next()?.contains('.')
        {
            return None;
        }
        files.push(file.to_string());
    }
    // Concatenated fragments that hide the directory name stay broad.
    if split_directory(text, &needle) {
        return None;
    }
    // Runtime path manipulation is not an exact import. Count every mention
    // against literal embeds, including escaped Rust source fixtures. A
    // second use of the same path outside an embed still widens the edge.
    let fixture_code = text.replace("\\\"", "\"");
    let mut literals: Vec<String> = embed_literals(&fixture_code)
        .into_iter()
        .map(|(_, literal)| literal)
        .collect();
    literals.extend(
        conditional_paths(&fixture_code)
            .into_iter()
            .map(|(path, _)| path),
    );
    let mut embedded: Vec<String> = literals
        .into_iter()
        .filter_map(|literal| {
            let at = literal.find(&needle)?;
            Some(literal.get(at..)?.to_string())
        })
        .collect();
    for file in &files {
        let position = embedded.iter().position(|path| path == file)?;
        embedded.remove(position);
    }
    Some(files)
}

/// Conditional path attributes name every alternative, regardless of the
/// feature selected by this host. Only the top-level `path = LITERAL`
/// attribute after cfg_attr's condition is recognized.
fn conditional_paths(code: &str) -> Vec<(String, bool)> {
    let mut paths = Vec::new();
    for rest in code.split("#[cfg_attr(").skip(1) {
        let mut at = 0usize;
        let mut depth = 1usize;
        let mut attribute = false;
        let mut target_recipe = false;
        let mut found = Vec::new();
        while let Some(tail) = rest.get(at..).filter(|s| !s.is_empty()) {
            if let Some((_, len)) = string_literal_span(tail) {
                at = at.saturating_add(len);
                continue;
            }
            let Some(ch) = tail.chars().next() else {
                break;
            };
            match ch {
                '(' => depth = depth.saturating_add(1),
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        if tail
                            .get(1..)
                            .is_some_and(|s| s.trim_start().starts_with(']'))
                        {
                            paths.extend(found);
                        }
                        break;
                    }
                }
                ',' if depth == 1 => {
                    if !attribute {
                        let condition: String = rest
                            .get(..at)
                            .unwrap_or("")
                            .chars()
                            .filter(|c| !c.is_whitespace())
                            .collect();
                        target_recipe = condition == "feature=\"target-recipe\"";
                    }
                    attribute = true;
                }
                _ => {}
            }
            if attribute
                && depth == 1
                && tail.starts_with("path")
                && !rest
                    .get(..at)
                    .and_then(|s| s.chars().next_back())
                    .is_some_and(super::is_name_char)
            {
                if let Some(value) = tail.get(4..).and_then(|s| s.trim_start().strip_prefix('=')) {
                    if let Some((literal, _)) = string_literal_span(value.trim_start()) {
                        found.push((literal.to_string(), target_recipe));
                    }
                }
            }
            at = at.saturating_add(ch.len_utf8());
        }
    }
    paths
}

/// Joining literals must not hide an additional donor name beside a direct
/// import. A boundary inside the directory spelling makes its reads uncertain.
fn split_directory(text: &str, needle: &str) -> bool {
    let mut joined = String::new();
    let mut boundaries = Vec::new();
    for (index, part) in text.split('"').enumerate() {
        if index % 2 == 1 {
            boundaries.push(joined.len());
            joined.push_str(part);
        }
    }
    // A program name followed by an unrelated absolute argument is not a
    // source-directory spelling. Check slash-only splits within concat!
    // expressions, while splits inside the crate name remain uncertain.
    joined.match_indices(needle).any(|(at, _)| {
        boundaries
            .iter()
            .any(|b| at < *b && *b < at.saturating_add(needle.len().saturating_sub(1)))
    }) || concat_names_directory(text, needle)
}

fn concat_names_directory(text: &str, needle: &str) -> bool {
    for tail in text.split("concat!").skip(1) {
        let mut tail = tail.trim_start();
        let close = match tail.chars().next() {
            Some('(') => ')',
            Some('[') => ']',
            Some('{') => '}',
            _ => continue,
        };
        tail = tail.get(1..).unwrap_or("");
        let mut joined = String::new();
        loop {
            tail = tail.trim_start();
            if tail.starts_with(close) {
                if joined.contains(needle) {
                    return true;
                }
                break;
            }
            let Some((literal, len)) = string_literal_span(tail) else {
                break;
            };
            joined.push_str(literal);
            tail = tail.get(len..).unwrap_or("").trim_start();
            if let Some(next) = tail.strip_prefix(',') {
                tail = next;
            }
        }
    }
    false
}

/// The sole feature is off in the generated host commands. Roster path
/// dependencies cannot request features, and any fixture feature or richer
/// feature table prevents this exception at the caller.
pub(super) fn host_target_recipe_inactive(manifest: &str) -> bool {
    let mut in_features = false;
    let mut features = Vec::new();
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.starts_with('[') {
            in_features = line == "[features]";
        } else if in_features && !line.is_empty() {
            features.push(
                line.chars()
                    .filter(|c| !c.is_whitespace())
                    .collect::<String>(),
            );
        }
    }
    features == ["target-recipe=[]"]
}

/// Read local literal includes with their original directories, even when
/// they are not .rs files. Foreign roster sources have their own graph nodes;
/// non-roster shared sources are expanded here instead.
pub(super) fn consumer_inputs(
    root: &Path,
    reader: &str,
    files: Vec<PathBuf>,
    inactive_target: bool,
) -> Result<(String, bool), String> {
    let initial = files.clone();
    let mut pending = files;
    let mut visited = Vec::new();
    let mut text = String::new();
    let mut complete = true;
    while let Some(file) = pending.pop() {
        if visited.contains(&file) {
            continue;
        }
        if visited.len() >= 4096 && !initial.contains(&file) {
            complete = false;
            continue;
        }
        visited.push(file.clone());
        let bytes = match std::fs::read(&file) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !initial.contains(&file) => {
                complete = false;
                continue;
            }
            Err(e) => return Err(format!("{} could not be read: {e}", file.display())),
        };
        let Ok(raw) = String::from_utf8(bytes) else {
            continue;
        };
        let code = strip_comments(&raw);
        text.push_str(&code);
        text.push('\n');
        if has_computed_embed(&code) || has_nested_path_attribute(&code) {
            complete = false;
        }
        if !initial.contains(&file) && has_unresolved_modules(&code) {
            complete = false;
        }
        let Some(dir) = file
            .parent()
            .and_then(|p| p.strip_prefix(root).ok())
            .and_then(Path::to_str)
        else {
            complete = false;
            continue;
        };
        // Text and byte includes read data; only include! and path modules
        // compile further imports. Their literal data paths remain in text.
        let mut embeds: Vec<String> = code_embed_literals(&code)
            .into_iter()
            .filter(|(at, _)| {
                code.get(*at..)
                    .is_some_and(|s| s.starts_with("include!") || s.starts_with("#[path"))
            })
            .map(|(_, p)| p)
            .collect();
        embeds.extend(
            conditional_paths(&code)
                .into_iter()
                .filter_map(|(path, target)| (!(target && inactive_target)).then_some(path)),
        );
        for literal in embeds {
            if literal.contains('\\') {
                complete = false;
                continue;
            }
            let Some(path) = normalize_join(dir, &literal) else {
                complete = false;
                continue;
            };
            let top = path.split('/').next().unwrap_or("");
            if top != reader
                && top.starts_with("td-")
                && root.join(top).join("Cargo.toml").is_file()
            {
                continue;
            }
            let path = root.join(path);
            if path
                .ancestors()
                .take_while(|p| *p != root)
                .any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()))
            {
                complete = false;
                continue;
            }
            pending.push(path);
        }
    }
    Ok((text, complete))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> crate::test_temp::TempDir {
        let root = crate::test_temp::TempDir::new("td-shared-source-scope").unwrap();
        std::fs::create_dir_all(root.join("td-aa/src")).unwrap();
        for name in ["shared.rs", "private.rs", "child.rs"] {
            std::fs::write(root.join("td-aa/src").join(name), "pub fn value() {}\n").unwrap();
        }
        root
    }

    fn reads(root: &Path, text: &str, changed: &str) -> bool {
        may_read_changed(root, text, "td-aa", &[changed.to_string()]).unwrap()
    }

    #[test]
    fn exact_shared_file_does_not_select_a_private_source_change() {
        let root = fixture();
        for text in [
            "#[path = \"../../td-aa/src/shared.rs\"]\nmod shared;",
            "include_str!(\n r#\"../../td-aa/src/shared.rs\"#\n)",
            "#[cfg_attr(\nnot(feature = \"target\"),\npath = \"../../td-aa/src/shared.rs\"\n)]\nmod shared;",
            "\"#[path = \\\"../../td-aa/src/shared.rs\\\"]\"",
        ] {
            assert!(!reads(&root, text, "td-aa/src/private.rs"), "{text}");
            assert!(reads(&root, text, "td-aa/src/shared.rs"), "{text}");
        }
    }

    #[test]
    fn explicit_nested_modules_and_includes_are_followed() {
        let root = fixture();
        std::fs::write(
            root.join("td-aa/src/shared.rs"),
            "#[path = \"child.rs\"]\npub(crate) mod child;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("td-aa/src/child.rs"),
            "include_str!(\"payload\");\n",
        )
        .unwrap();
        std::fs::write(root.join("td-aa/src/payload"), "payload").unwrap();
        let text = "include!(\"../td-aa/src/shared.rs\")";
        assert!(!reads(&root, text, "td-aa/src/private.rs"));
        for file in ["shared.rs", "child.rs", "payload"] {
            assert!(reads(&root, text, &format!("td-aa/src/{file}")));
        }
        // A cycle terminates without losing any of its inputs.
        std::fs::write(
            root.join("td-aa/src/child.rs"),
            "include!(\"shared.rs\");\n",
        )
        .unwrap();
        assert!(!reads(&root, text, "td-aa/src/private.rs"));
        assert!(reads(&root, text, "td-aa/src/child.rs"));
    }

    #[test]
    fn uncertain_reads_keep_the_whole_crate_edge() {
        let root = fixture();
        for text in [
            "root.join(\"td-aa/src\").join(file)",
            "root.join(\"td-aa/src/shared.rs\").with_file_name(\"private.rs\")",
            "include!(\"td-aa/src/shared.rs\"); read(\"td-aa/src/shared.rs\")",
            "concat!(\"td-aa/\", \"src/shared.rs\")",
            "format!(\"td-aa/src/{file}.rs\")",
            "\"td-aa/src/shared.rs{suffix}\"",
            "\"td-aa/src/../src/shared.rs\"",
            "\"td-aa/src/shared.rs\\x2fchild\"",
            "\"td-aa/src/missing.rs\"",
        ] {
            assert!(reads(&root, text, "td-aa/src/private.rs"), "{text}");
        }
        for body in [
            "mod child;",
            "include!(concat!(\"child\", \".rs\"));",
            "#[cfg_attr(test, path = \"child.rs\")]\nmod child;",
            "mod nested {\n    #[path = \"child.rs\"]\n    mod child;\n}",
        ] {
            std::fs::write(root.join("td-aa/src/shared.rs"), body).unwrap();
            assert!(
                reads(
                    &root,
                    "include!(\"td-aa/src/shared.rs\")",
                    "td-aa/src/private.rs"
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn mixed_direct_and_assembled_imports_remain_broad() {
        let root = fixture();
        for extra in [
            "include_str!(concat!(\"../../td-\", \"aa/src/private.rs\"));",
            "read_to_string(concat!(\"../../td-\", \"aa/src/private.rs\"));",
            "read_to_string(concat!(\"td-aa\", \"/src/private.rs\"));",
        ] {
            let text = format!("include_str!(\"../../td-aa/src/shared.rs\"); {extra}");
            assert!(reads(&root, &text, "td-aa/src/private.rs"), "{extra}");
        }
        let program = "include_str!(\"../../td-aa/src/shared.rs\"); Command::new(\"td-aa\").arg(\"/run/state\");";
        assert!(!reads(&root, program, "td-aa/src/private.rs"));
    }

    #[test]
    fn consumer_local_compiled_includes_keep_their_original_directory() {
        let root = fixture();
        std::fs::create_dir_all(root.join("td-reader/src")).unwrap();
        let main = root.join("td-reader/src/main.rs");
        std::fs::write(
            &main,
            "include_str!(\"../../td-aa/src/shared.rs\"); include!(\"extra.inc\");",
        )
        .unwrap();
        std::fs::write(
            root.join("td-reader/src/extra.inc"),
            "include_str!(\"../../td-aa/src/private.rs\");",
        )
        .unwrap();
        let (text, complete) =
            consumer_inputs(&root, "td-reader", vec![main.clone()], false).unwrap();
        assert!(complete);
        assert!(reads(&root, &text, "td-aa/src/private.rs"));
        std::fs::remove_file(root.join("td-reader/src/extra.inc")).unwrap();
        assert!(
            !consumer_inputs(&root, "td-reader", vec![main], false)
                .unwrap()
                .1
        );
    }

    #[test]
    fn inline_path_modules_cannot_use_the_containing_files_directory() {
        let root = fixture();
        std::fs::create_dir_all(root.join("td-reader/src/nested")).unwrap();
        let main = root.join("td-reader/src/main.rs");
        std::fs::write(
            &main,
            r#"include_str!("../../td-aa/src/shared.rs");
mod nested { #[path = "extra.inc"] pub mod extra; }"#,
        )
        .unwrap();
        // The wrong-base file exists, so a missing-file fallback cannot help.
        std::fs::write(root.join("td-reader/src/extra.inc"), "").unwrap();
        std::fs::write(
            root.join("td-reader/src/nested/extra.inc"),
            r#"include_str!("../../../td-aa/src/private.rs");"#,
        )
        .unwrap();
        let (_, complete) = consumer_inputs(&root, "td-reader", vec![main.clone()], false).unwrap();
        assert!(!complete);
        std::fs::write(
            &main,
            r#"include_str!("../../td-aa/src/shared.rs");
mod nested { #[cfg_attr(test, path = "extra.inc")] pub mod extra; }"#,
        )
        .unwrap();
        assert!(
            !consumer_inputs(&root, "td-reader", vec![main], false)
                .unwrap()
                .1
        );
    }

    #[test]
    fn only_the_verified_host_feature_shape_skips_target_paths() {
        assert!(host_target_recipe_inactive(
            "[features]\ntarget-recipe = []\n"
        ));
        for manifest in [
            "[features]\ntarget-recipe = []\ndefault = [\"target-recipe\"]",
            "[features]\ntarget-recipe = []\nfixture = []",
            "[features]\ntarget-recipe = [\"other\"]",
            "[package]\nname = \"td-aa\"",
        ] {
            assert!(!host_target_recipe_inactive(manifest), "{manifest}");
        }
        let root = fixture();
        let file = root.join("td-aa/src/main.rs");
        std::fs::write(&file, "#[cfg_attr(feature = \"target-recipe\", path = \"generated/missing.rs\")]\nmod module;\n").unwrap();
        assert!(
            consumer_inputs(&root, "td-aa", vec![file.clone()], true)
                .unwrap()
                .1
        );
        assert!(
            !consumer_inputs(&root, "td-aa", vec![file.clone()], false)
                .unwrap()
                .1
        );
        std::fs::write(
            &file,
            "#[cfg_attr(test, path = \"generated/missing.rs\")]\nmod module;\n",
        )
        .unwrap();
        assert!(!consumer_inputs(&root, "td-aa", vec![file], true).unwrap().1);
    }

    #[test]
    fn a_symlink_cannot_establish_a_file_scope() {
        let root = fixture();
        std::os::unix::fs::symlink("private.rs", root.join("td-aa/src/alias.rs")).unwrap();
        assert!(reads(
            &root,
            "\"td-aa/src/alias.rs\"",
            "td-aa/src/private.rs"
        ));
    }
}
