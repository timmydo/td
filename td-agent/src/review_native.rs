//! Host review build tools, prepared before paid completions.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use td_json::Json;

const PATH_VARS: &[&str] = &[
    "C_INCLUDE_PATH",
    "CPLUS_INCLUDE_PATH",
    "CPATH",
    "LIBRARY_PATH",
    "PKG_CONFIG_PATH",
    "PKG_CONFIG_LIBDIR",
];
const TOOL_VARS: &[&str] = &["CC", "CXX", "AR", "CMAKE"];

#[derive(Default)]
pub(crate) struct Native {
    pub env: Vec<(String, String)>,
}

fn system(path: &Path) -> bool {
    path.is_absolute()
        && crate::jail::SYSTEM_TREES
            .iter()
            .any(|root| path.starts_with(root))
}

fn invocation(path: &Path) -> Option<(PathBuf, PathBuf)> {
    let named = fs::canonicalize(path.parent()?)
        .ok()?
        .join(path.file_name()?);
    let target = fs::canonicalize(&named).ok()?;
    Some((named, target))
}

fn executable(path: &Path) -> Option<PathBuf> {
    let (named, target) = invocation(path)?;
    let meta = fs::metadata(&target).ok()?;
    (system(&named) && system(&target) && meta.is_file() && meta.mode() & 0o111 != 0)
        .then_some(named)
}

fn find(
    name: &str,
    directories: &[PathBuf],
    aliases: &[crate::host_path::Alias],
) -> Option<PathBuf> {
    if Path::new(name).is_absolute() {
        return executable(Path::new(name));
    }
    if name.contains('/') {
        return None;
    }
    if aliases.iter().any(|alias| alias.name == name) {
        return Some(Path::new(crate::jail::ALIAS_DIR).join(name));
    }
    directories
        .iter()
        .find_map(|dir| executable(&dir.join(name)))
}

fn path_list(value: &std::ffi::OsStr) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    let mut paths = Vec::new();
    for part in std::env::split_paths(value) {
        if part.as_os_str().is_empty() {
            continue;
        }
        if !part.is_absolute() {
            return None;
        }
        let resolved = fs::canonicalize(part).ok()?;
        if !system(&resolved) || !resolved.is_dir() {
            return None;
        }
        if !paths.contains(&resolved) {
            paths.push(resolved);
        }
        if paths.len() > 32 {
            return None;
        }
    }
    let value = std::env::join_paths(paths).ok()?.into_string().ok()?;
    (value.len() <= 8192).then_some(value)
}

pub(crate) fn decode_environment(value: Option<&Json>) -> Result<Vec<(String, String)>, String> {
    let Some(value) = value.filter(|v| !matches!(v, Json::Null)) else {
        return Ok(Vec::new());
    };
    let Json::Obj(fields) = value else {
        return Err("review build environment must be an object".into());
    };
    if fields.len() > PATH_VARS.len() + TOOL_VARS.len() {
        return Err("too many review build variables".into());
    }
    let mut out = Vec::new();
    for (key, value) in fields {
        if !PATH_VARS.contains(&key.as_str()) && !TOOL_VARS.contains(&key.as_str()) {
            return Err(format!("review build environment cannot override {key}"));
        }
        let text = value
            .as_str()
            .filter(|v| v.len() <= 8192 && !v.contains(['\0', '\n', '\r']))
            .ok_or("invalid review build environment value")?;
        out.push((key.clone(), text.into()));
    }
    Ok(out)
}

pub(crate) fn prepare(
    workspace: &mut crate::review_workspace::Workspace,
    journal: &mut crate::review_log::Journal,
) -> Result<String, String> {
    let mut directories = crate::jail::system_path(std::env::var_os("PATH").as_deref());
    for directory in crate::jail::system_path(Some(std::ffi::OsStr::new(
        "/usr/local/bin:/usr/bin:/bin:/run/current-system/profile/bin:/run/current-system/sw/bin",
    ))) {
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    let aliases = crate::jail::host_aliases();
    let mut limitations = Vec::new();
    for key in PATH_VARS {
        if let Some(value) = std::env::var_os(key) {
            if !value.is_empty() && std::env::split_paths(&value).any(|p| p.as_os_str().is_empty())
            {
                limitations.push(format!("{key}: empty search entries omitted instead of rebasing the parent's working directory"));
            }
            if let Some(value) = path_list(&value) {
                workspace.native.env.push(((*key).into(), value));
            } else {
                limitations.push(format!(
                    "{key} omitted: not an absolute mounted system directory list"
                ));
            }
        }
    }
    for key in TOOL_VARS {
        if let Some(value) = std::env::var_os(key) {
            if let Some(path) = value.to_str().and_then(|v| find(v, &directories, &aliases)) {
                workspace
                    .native
                    .env
                    .push(((*key).into(), path.display().to_string()));
            } else {
                limitations.push(format!(
                    "{key} omitted: not one executable in the mounted host tool closure"
                ));
            }
        }
    }
    journal.event(
        "native_build_environment",
        Json::Obj(
            workspace
                .native
                .env
                .iter()
                .map(|(k, v)| (k.clone(), Json::Str(v.clone())))
                .collect(),
        ),
    )?;
    for (program, filename, text, args) in [
        (
            "cc",
            "native-probe.c",
            "#include <stdio.h>\nint main(void) { return puts(\"review\") < 0; }\n",
            Vec::<String>::new(),
        ),
        (
            "rustc",
            "native-probe.rs",
            "fn main() { println!(\"review\"); }\n",
            vec![
                "--edition=2021".into(),
                "--crate-name=review_native_probe".into(),
            ],
        ),
    ] {
        let source = workspace.scratch.join(filename);
        fs::write(&source, text).map_err(|e| e.to_string())?;
        let mut words = args;
        words.extend([
            source.display().to_string(),
            "-o".into(),
            workspace
                .scratch
                .join(format!("{filename}.out"))
                .display()
                .to_string(),
        ]);
        let result = workspace
            .native_logged(program, words, |kind, bytes| {
                if kind == "tool_output_bytes_hex" {
                    journal.raw("preflight_output_bytes_hex", bytes)
                } else {
                    journal.text(
                        "preflight_output",
                        std::str::from_utf8(bytes).map_err(|e| e.to_string())?,
                    )
                }
            })
            .unwrap_or_else(|e| e);
        let available = result.starts_with("[exit status 0]");
        journal.event(
            "native_build_preflight",
            Json::Obj(vec![
                ("program".into(), Json::Str(program.into())),
                ("available".into(), Json::Bool(available)),
                ("result".into(), Json::Str(result.clone())),
            ]),
        )?;
        if !available {
            limitations.push(format!(
                "{program} compile/link unavailable: {}",
                result.chars().take(1500).collect::<String>()
            ));
        }
    }
    Ok(if limitations.is_empty() {
        "Native C and Rust compile/link probes passed; standard compiler commands and validated parent build paths are available.".into()
    } else {
        format!("Native build limitations: {}", limitations.join("\n"))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn compiler_selection_preserves_driver_mode_and_resolves_parent_profiles() {
        use std::os::unix::fs::symlink;
        let root = td_fs::TempDir::new("compiler-name").unwrap();
        let tools = root.path().join("tools");
        fs::create_dir(&tools).unwrap();
        fs::write(tools.join("clang"), "driver").unwrap();
        symlink("clang", tools.join("clang++")).unwrap();
        symlink("tools", root.path().join("profile")).unwrap();
        let (named, target) = invocation(&root.path().join("profile/clang++")).unwrap();
        assert_eq!(named, fs::canonicalize(&tools).unwrap().join("clang++"));
        assert_eq!(target, fs::canonicalize(tools.join("clang")).unwrap());
        assert_ne!(named, target);
        assert!(executable(&root.path().join("profile/clang++")).is_none());
    }
    #[test]
    fn empty_search_values_preserve_pkg_config_default_suppression() {
        assert_eq!(path_list(std::ffi::OsStr::new("")), Some(String::new()));
        let directory = crate::jail::SYSTEM_TREES
            .iter()
            .find_map(|p| fs::canonicalize(p).ok().filter(|p| system(p) && p.is_dir()))
            .unwrap();
        let list = format!("{}:", directory.display());
        assert_eq!(
            path_list(std::ffi::OsStr::new(&list)),
            Some(directory.display().to_string())
        );
        let decoded = decode_environment(Some(&Json::Obj(vec![(
            "PKG_CONFIG_LIBDIR".into(),
            Json::Str(String::new()),
        )])))
        .unwrap();
        assert_eq!(decoded, vec![("PKG_CONFIG_LIBDIR".into(), String::new())]);
    }

    #[test]
    fn build_environment_does_not_override_confinement_or_import_secrets() {
        for key in [
            "PATH",
            "HOME",
            "CARGO_HOME",
            "CARGO_NET_OFFLINE",
            "OPENROUTER_API_KEY",
            "RUSTFLAGS",
            "TD_AGENT_REVIEW_COMPILER",
        ] {
            assert!(decode_environment(Some(&Json::Obj(vec![(
                key.into(),
                Json::Str("x".into())
            )])))
            .is_err());
        }
        assert!(path_list(std::ffi::OsStr::new("relative/include")).is_none());
        assert!(path_list(std::ffi::OsStr::new("/home")).is_none());
        assert!(decode_environment(Some(&Json::Obj(vec![(
            "CC".into(),
            Json::Str("bad\0path".into())
        )])))
        .is_err());
    }
}
