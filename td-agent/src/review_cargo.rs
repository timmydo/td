//! Typed Cargo tests: no pipelines can obscure Cargo's exit status.
use std::path::Path;
use td_json::Json;

pub(crate) fn definition() -> Json {
    Json::Obj(vec![
        ("type".into(), Json::Str("function".into())),
        ("function".into(), Json::Obj(vec![
            ("name".into(), Json::Str("cargo_test".into())),
            ("description".into(), Json::Str("Run cargo test directly, offline and locked, with private build output and the prepared test runner. Cargo's actual exit status is preserved; no shell or output pipeline. Maximum 600 seconds. Output may be shortened, but full streamed bytes remain in the trace. Use this instead of piping cargo test through tail/grep. Missing inputs are limitations, not passing tests.".into())),
            ("parameters".into(), Json::Obj(vec![
                ("type".into(), Json::Str("object".into())),
                ("properties".into(), Json::Obj(vec![
                    ("manifest".into(), property("string", "Cargo.toml path; relative paths resolve from Source. Defaults to Cargo.toml.")),
                    ("filter".into(), property("string", "One test-name substring, not a flag.")),
                    ("lib".into(), property("boolean", "Test only the library.")),
                    ("test".into(), property("string", "One integration-test target name.")),
                    ("no_run".into(), property("boolean", "Compile tests without executing them.")),
                ])),
                ("additionalProperties".into(), Json::Bool(false)),
            ])),
        ])),
    ])
}

fn property(kind: &str, description: &str) -> Json {
    Json::Obj(vec![
        ("type".into(), Json::Str(kind.into())),
        ("description".into(), Json::Str(description.into())),
    ])
}

pub(crate) fn arguments(text: &str, checkout: &Path) -> Result<Vec<String>, String> {
    if text.len() > crate::tools::MAX_ARGUMENTS {
        return Err("cargo_test arguments exceed their bound".into());
    }
    let value = td_json::parse(text).map_err(|e| e.to_string())?;
    let Json::Obj(fields) = &value else {
        return Err("cargo_test needs an object".into());
    };
    for (name, _) in fields {
        if !["manifest", "filter", "lib", "test", "no_run"].contains(&name.as_str()) {
            return Err(format!("cargo_test has no parameter {name:?}"));
        }
    }
    let word = |name: &str| -> Result<Option<&str>, String> {
        value
            .get(name)
            .map(|v| {
                v.as_str()
                    .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'))
                    .ok_or_else(|| format!("invalid cargo_test {name}"))
            })
            .transpose()
    };
    let flag = |name: &str| -> Result<bool, String> {
        value
            .get(name)
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| format!("cargo_test {name} is not boolean"))
            })
            .transpose()
            .map(|v| v.unwrap_or(false))
    };
    let manifest = checkout.join(word("manifest")?.unwrap_or("Cargo.toml"));
    let mut args = vec![
        "test".into(),
        "--offline".into(),
        "--locked".into(),
        "--manifest-path".into(),
        manifest.display().to_string(),
    ];
    if flag("lib")? {
        args.push("--lib".into());
    }
    if flag("no_run")? {
        args.push("--no-run".into());
    }
    if let Some(test) = word("test")? {
        if test.starts_with('-') {
            return Err("cargo_test target is not a flag".into());
        }
        args.extend(["--test".into(), test.into()]);
    }
    if let Some(filter) = word("filter")? {
        if filter.starts_with('-') {
            return Err("cargo_test filter is not a flag".into());
        }
        args.push(filter.into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn arguments_preserve_literal_paths_and_refuse_cargo_overrides() {
        let args = arguments(
            r#"{"manifest":"crate with $()/Cargo.toml","filter":"a|b","lib":true}"#,
            Path::new("/source"),
        )
        .unwrap();
        assert!(args.contains(&"/source/crate with $()/Cargo.toml".into()));
        assert_eq!(args.last().map(String::as_str), Some("a|b"));
        for text in [
            r#"{"args":["--config","x"]}"#,
            r#"{"filter":"--config"}"#,
            r#"{"lib":"true"}"#,
        ] {
            assert!(arguments(text, Path::new("/source")).is_err());
        }
    }
}
