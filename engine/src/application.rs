//! The build-time declaration for a shipped application package.
//!
//! `APPLICATIONS.md` section B.2 keeps this manifest separate from both the
//! jail spec and the operator's permission file. Recipes declare its authored
//! fields and the builder binds final recipe identity/provenance before writing
//! it; the later spec compiler consumes the validated values rather than
//! treating the manifest as a mount plan.

use crate::json::Json;
use std::collections::BTreeMap;

const MAX_APPLICATION_NAME_BYTES: usize = 32;
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;
const MAX_VERSION_BYTES: usize = 128;
const MAX_ALIAS_BYTES: usize = 255;
const MAX_ENTRY_BYTES: usize = 4096;
const MAX_ENVIRONMENT_ENTRIES: usize = 128;
const MAX_ENVIRONMENT_NAME_BYTES: usize = 128;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationProvenance {
    Source,
    Foreign,
}

impl ApplicationProvenance {
    fn parse(value: &str) -> Result<ApplicationProvenance, String> {
        match value {
            "source" => Ok(ApplicationProvenance::Source),
            "foreign" => Ok(ApplicationProvenance::Foreign),
            _ => Err(format!(
                "provenance must be `source' or `foreign', not {value:?}"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            ApplicationProvenance::Source => "source",
            ApplicationProvenance::Foreign => "foreign",
        }
    }
}

/// Recipe-authored application fields. Identity, version and provenance are
/// deliberately absent: the derivation assembler takes those from the final
/// recipe when it renders the package manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationDeclaration {
    alias: Option<String>,
    runtime: String,
    entry: String,
    environment: BTreeMap<String, String>,
}

impl ApplicationDeclaration {
    pub fn new(runtime: &str, entry: &str) -> Result<ApplicationDeclaration, String> {
        validate_application_name(runtime).map_err(|reason| format!("runtime: {reason}"))?;
        validate_entry(entry)?;
        Ok(ApplicationDeclaration {
            alias: None,
            runtime: runtime.to_string(),
            entry: entry.to_string(),
            environment: BTreeMap::new(),
        })
    }

    pub fn with_alias(mut self, alias: &str) -> Result<ApplicationDeclaration, String> {
        if self.alias.is_some() {
            return Err("duplicate application declaration alias".into());
        }
        validate_alias(alias)?;
        self.alias = Some(alias.to_string());
        Ok(self)
    }

    pub fn with_environment(
        mut self,
        name: &str,
        value: &str,
    ) -> Result<ApplicationDeclaration, String> {
        insert_environment(&mut self.environment, name, value)?;
        Ok(self)
    }

    /// Strict recipe-JSON representation consumed by the derivation assembler.
    pub fn from_json(value: &Json) -> Result<ApplicationDeclaration, String> {
        let Json::Obj(fields) = value else {
            return Err("application declaration is not a JSON object".into());
        };
        let mut runtime = None;
        let mut entry = None;
        let mut alias = None;
        let mut environment = None;
        for (key, value) in fields {
            match key.as_str() {
                "runtime" => set_json_string(&mut runtime, key, value)?,
                "entry" => set_json_string(&mut entry, key, value)?,
                "alias" => set_json_string(&mut alias, key, value)?,
                "environment" => {
                    if environment.is_some() {
                        return Err("duplicate application declaration key \"environment\"".into());
                    }
                    environment = Some(parse_environment_json(value)?);
                }
                _ => return Err(format!("unknown application declaration key {key:?}")),
            }
        }
        let runtime = runtime.ok_or("application declaration is missing `runtime'")?;
        let entry = entry.ok_or("application declaration is missing `entry'")?;
        let mut declaration = ApplicationDeclaration::new(&runtime, &entry)?;
        if let Some(alias) = alias {
            declaration = declaration.with_alias(&alias)?;
        }
        if let Some(environment) = environment {
            declaration.environment = environment;
        }
        Ok(declaration)
    }

    pub fn to_json(&self) -> Json {
        let mut fields = vec![
            ("runtime".into(), Json::Str(self.runtime.clone())),
            ("entry".into(), Json::Str(self.entry.clone())),
        ];
        if let Some(alias) = &self.alias {
            fields.push(("alias".into(), Json::Str(alias.clone())));
        }
        if !self.environment.is_empty() {
            fields.push((
                "environment".into(),
                Json::Obj(
                    self.environment
                        .iter()
                        .map(|(name, value)| (name.clone(), Json::Str(value.clone())))
                        .collect(),
                ),
            ));
        }
        Json::Obj(fields)
    }

    /// Bind authored fields to the final recipe identity and trust answer.
    pub fn manifest(
        &self,
        name: &str,
        version: &str,
        provenance: ApplicationProvenance,
    ) -> Result<ApplicationManifest, String> {
        let mut manifest =
            ApplicationManifest::new(name, version, &self.runtime, &self.entry, provenance)?;
        if let Some(alias) = &self.alias {
            manifest = manifest.with_alias(alias)?;
        }
        for (environment_name, value) in &self.environment {
            manifest = manifest.with_environment(environment_name, value)?;
        }
        Ok(manifest)
    }

    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }

    pub fn environment(&self) -> impl Iterator<Item = (&str, &str)> {
        self.environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationManifest {
    name: String,
    version: String,
    alias: Option<String>,
    runtime: String,
    entry: String,
    provenance: ApplicationProvenance,
    environment: BTreeMap<String, String>,
}

impl ApplicationManifest {
    fn new(
        name: &str,
        version: &str,
        runtime: &str,
        entry: &str,
        provenance: ApplicationProvenance,
    ) -> Result<ApplicationManifest, String> {
        validate_application_name(name)?;
        validate_version(version)?;
        validate_application_name(runtime).map_err(|reason| format!("runtime: {reason}"))?;
        validate_entry(entry)?;
        let manifest = ApplicationManifest {
            name: name.to_string(),
            version: version.to_string(),
            alias: None,
            runtime: runtime.to_string(),
            entry: entry.to_string(),
            provenance,
            environment: BTreeMap::new(),
        };
        manifest.ensure_size()?;
        Ok(manifest)
    }

    fn with_alias(mut self, alias: &str) -> Result<ApplicationManifest, String> {
        validate_alias(alias)?;
        self.alias = Some(alias.to_string());
        self.ensure_size()?;
        Ok(self)
    }

    fn with_environment(mut self, name: &str, value: &str) -> Result<ApplicationManifest, String> {
        insert_environment(&mut self.environment, name, value)?;
        self.ensure_size()?;
        Ok(self)
    }

    pub fn parse(text: &str) -> Result<ApplicationManifest, String> {
        parse_manifest(text)
    }

    /// Canonical bytes for the package's `manifest` file.
    pub fn to_keyfile(&self) -> String {
        let mut out = String::new();
        push_key(&mut out, "name", &self.name);
        push_key(&mut out, "version", &self.version);
        if let Some(alias) = &self.alias {
            push_key(&mut out, "alias", alias);
        }
        push_key(&mut out, "runtime", &self.runtime);
        push_key(&mut out, "entry", &self.entry);
        push_key(&mut out, "provenance", self.provenance.as_str());
        if !self.environment.is_empty() {
            out.push_str("\n[Environment]\n");
            for (name, value) in &self.environment {
                push_key(&mut out, name, value);
            }
        }
        out
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn alias(&self) -> Option<&str> {
        self.alias.as_deref()
    }

    pub fn runtime(&self) -> &str {
        &self.runtime
    }

    pub fn entry(&self) -> &str {
        &self.entry
    }

    pub fn provenance(&self) -> ApplicationProvenance {
        self.provenance
    }

    pub fn environment(&self) -> impl Iterator<Item = (&str, &str)> {
        self.environment
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    fn ensure_size(&self) -> Result<(), String> {
        let size = self.to_keyfile().len();
        if size > MAX_MANIFEST_BYTES {
            return Err(format!(
                "application manifest would be {size} bytes; the limit is {MAX_MANIFEST_BYTES}"
            ));
        }
        Ok(())
    }
}

/// Validate the one short identity used for launcher names, state paths and
/// policy lookup. Runtime package names share the same filesystem-safe form.
pub fn validate_application_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("application name is empty".into());
    }
    if name.len() > MAX_APPLICATION_NAME_BYTES {
        return Err(format!(
            "application name is {} bytes; the limit is {MAX_APPLICATION_NAME_BYTES}",
            name.len()
        ));
    }
    if name.starts_with('-') {
        return Err("application name may not begin with `-'".into());
    }
    if name == "." {
        return Err("application name may not be `.'".into());
    }
    if name.contains("..") {
        return Err("application name may not contain `..'".into());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err("application name must use only ASCII letters, digits, `.`, `_` or `-'".into());
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), String> {
    validate_scalar("version", version, MAX_VERSION_BYTES, false)
}

fn validate_alias(alias: &str) -> Result<(), String> {
    validate_scalar("alias", alias, MAX_ALIAS_BYTES, false)?;
    let mut components = 0usize;
    for component in alias.split('.') {
        components += 1;
        let mut bytes = component.bytes();
        let Some(first) = bytes.next() else {
            return Err("alias contains an empty reverse-DNS component".into());
        };
        if !(first.is_ascii_alphabetic() || first == b'_') {
            return Err(format!(
                "alias component {component:?} must begin with an ASCII letter or `_`"
            ));
        }
        if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')) {
            return Err(format!(
                "alias component {component:?} must use only ASCII letters, digits, `_` or `-'"
            ));
        }
    }
    if components < 3 {
        return Err("alias must have at least three reverse-DNS components".into());
    }
    Ok(())
}

pub(crate) fn validate_entry(entry: &str) -> Result<(), String> {
    validate_scalar("entry", entry, MAX_ENTRY_BYTES, false)?;
    let Some(relative) = entry.strip_prefix("/app/") else {
        return Err("entry must be an absolute path below `/app/'".into());
    };
    for component in relative.split('/') {
        if component.is_empty() {
            return Err("entry may not contain an empty path component".into());
        }
        if matches!(component, "." | "..") {
            return Err(format!("entry may not contain a {component:?} component"));
        }
    }
    Ok(())
}

fn set_json_string(slot: &mut Option<String>, key: &str, value: &Json) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("duplicate application declaration key {key:?}"));
    }
    let value = value
        .as_str()
        .ok_or_else(|| format!("application declaration {key:?} is not a string"))?;
    *slot = Some(value.to_string());
    Ok(())
}

fn parse_environment_json(value: &Json) -> Result<BTreeMap<String, String>, String> {
    let Json::Obj(entries) = value else {
        return Err("application declaration environment is not a JSON object".into());
    };
    let mut environment = BTreeMap::new();
    for (name, value) in entries {
        let value = value
            .as_str()
            .ok_or_else(|| format!("application environment value for {name:?} is not a string"))?;
        insert_environment(&mut environment, name, value)?;
    }
    Ok(environment)
}

fn insert_environment(
    environment: &mut BTreeMap<String, String>,
    name: &str,
    value: &str,
) -> Result<(), String> {
    validate_environment_name(name)?;
    validate_environment_value(value)?;
    if environment.contains_key(name) {
        return Err(format!("duplicate environment key {name:?}"));
    }
    if environment.len() >= MAX_ENVIRONMENT_ENTRIES {
        return Err(format!(
            "an application manifest may carry at most {MAX_ENVIRONMENT_ENTRIES} environment entries"
        ));
    }
    environment.insert(name.to_string(), value.to_string());
    Ok(())
}

pub(crate) fn validate_environment_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("environment name is empty".into());
    }
    if name.len() > MAX_ENVIRONMENT_NAME_BYTES {
        return Err(format!(
            "environment name is {} bytes; the limit is {MAX_ENVIRONMENT_NAME_BYTES}",
            name.len()
        ));
    }
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err("environment name is empty".into());
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return Err(format!(
            "environment name {name:?} must begin with an ASCII letter or `_`"
        ));
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') {
        return Err(format!(
            "environment name {name:?} must use only ASCII letters, digits or `_`"
        ));
    }
    if name.starts_with("TD_") {
        return Err(format!("environment name {name:?} is reserved by td"));
    }
    Ok(())
}

pub(crate) fn validate_environment_value(value: &str) -> Result<(), String> {
    validate_scalar(
        "environment value",
        value,
        MAX_ENVIRONMENT_VALUE_BYTES,
        true,
    )
}

fn validate_scalar(label: &str, value: &str, max: usize, may_be_empty: bool) -> Result<(), String> {
    if !may_be_empty && value.is_empty() {
        return Err(format!("{label} is empty"));
    }
    if value.len() > max {
        return Err(format!(
            "{label} is {} bytes; the limit is {max}",
            value.len()
        ));
    }
    if value.trim() != value {
        return Err(format!("{label} may not begin or end with whitespace"));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{label} may not contain a control character"));
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Declaration,
    Environment,
}

#[derive(Default)]
struct Fields {
    name: Option<String>,
    version: Option<String>,
    alias: Option<String>,
    runtime: Option<String>,
    entry: Option<String>,
    provenance: Option<ApplicationProvenance>,
    environment: BTreeMap<String, String>,
}

fn parse_manifest(text: &str) -> Result<ApplicationManifest, String> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "application manifest is {} bytes; the limit is {MAX_MANIFEST_BYTES}",
            text.len()
        ));
    }
    if text.is_empty() {
        return Err("application manifest is empty".into());
    }
    if !text.ends_with('\n') {
        return Err("application manifest lacks a trailing newline".into());
    }
    if text.contains('\r') {
        return Err("application manifest contains a carriage return".into());
    }
    if text.contains('\0') {
        return Err("application manifest contains a NUL byte".into());
    }

    let mut fields = Fields::default();
    let mut section = Section::Declaration;
    let mut saw_environment = false;
    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if line != "[Environment]" {
                return Err(at_line(number, &format!("unknown section {line:?}")));
            }
            if saw_environment {
                return Err(at_line(number, "duplicate [Environment] section"));
            }
            saw_environment = true;
            section = Section::Environment;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(at_line(number, "expected key=value or [Environment]"));
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            return Err(at_line(number, "empty key"));
        }
        match section {
            Section::Declaration => apply_declaration(&mut fields, key, value, number)?,
            Section::Environment => apply_environment(&mut fields, key, value, number)?,
        }
    }

    let name = required(fields.name, "name")?;
    let version = required(fields.version, "version")?;
    let runtime = required(fields.runtime, "runtime")?;
    let entry = required(fields.entry, "entry")?;
    let provenance = fields
        .provenance
        .ok_or_else(|| "application manifest is missing `provenance'".to_string())?;
    let mut manifest = ApplicationManifest::new(&name, &version, &runtime, &entry, provenance)?;
    if let Some(alias) = fields.alias {
        manifest = manifest.with_alias(&alias)?;
    }
    for (name, value) in fields.environment {
        manifest = manifest.with_environment(&name, &value)?;
    }
    Ok(manifest)
}

fn apply_declaration(
    fields: &mut Fields,
    key: &str,
    value: &str,
    line: usize,
) -> Result<(), String> {
    match key {
        "name" => {
            validate_application_name(value).map_err(|reason| at_line(line, &reason))?;
            set_once(&mut fields.name, key, value, line)
        }
        "version" => {
            validate_version(value).map_err(|reason| at_line(line, &reason))?;
            set_once(&mut fields.version, key, value, line)
        }
        "alias" => {
            validate_alias(value).map_err(|reason| at_line(line, &reason))?;
            set_once(&mut fields.alias, key, value, line)
        }
        "runtime" => {
            validate_application_name(value)
                .map_err(|reason| at_line(line, &format!("runtime: {reason}")))?;
            set_once(&mut fields.runtime, key, value, line)
        }
        "entry" => {
            validate_entry(value).map_err(|reason| at_line(line, &reason))?;
            set_once(&mut fields.entry, key, value, line)
        }
        "provenance" => {
            if fields.provenance.is_some() {
                return Err(at_line(line, "duplicate key `provenance'"));
            }
            fields.provenance =
                Some(ApplicationProvenance::parse(value).map_err(|reason| at_line(line, &reason))?);
            Ok(())
        }
        _ => Err(at_line(line, &format!("unknown declaration key {key:?}"))),
    }
}

fn apply_environment(
    fields: &mut Fields,
    key: &str,
    value: &str,
    line: usize,
) -> Result<(), String> {
    validate_environment_name(key).map_err(|reason| at_line(line, &reason))?;
    validate_environment_value(value).map_err(|reason| at_line(line, &reason))?;
    if fields.environment.contains_key(key) {
        return Err(at_line(line, &format!("duplicate environment key {key:?}")));
    }
    if fields.environment.len() >= MAX_ENVIRONMENT_ENTRIES {
        return Err(at_line(
            line,
            &format!(
                "an application manifest may carry at most {MAX_ENVIRONMENT_ENTRIES} environment entries"
            ),
        ));
    }
    fields
        .environment
        .insert(key.to_string(), value.to_string());
    Ok(())
}

fn set_once(slot: &mut Option<String>, key: &str, value: &str, line: usize) -> Result<(), String> {
    if slot.is_some() {
        return Err(at_line(line, &format!("duplicate key {key:?}")));
    }
    *slot = Some(value.to_string());
    Ok(())
}

fn required(value: Option<String>, key: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("application manifest is missing `{key}'"))
}

fn at_line(line: usize, message: &str) -> String {
    format!("application manifest line {line}: {message}")
}

fn push_key(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "name=firefox\nversion=141.0\nalias=org.mozilla.firefox\nruntime=freedesktop-sdk-24.08\nentry=/app/bin/firefox\nprovenance=foreign\n\n[Environment]\nMOZ_ENABLE_WAYLAND=1\nTZ=America/Los_Angeles\n";

    fn base() -> ApplicationManifest {
        ApplicationDeclaration::new("freedesktop-sdk-24.08", "/app/bin/firefox")
            .unwrap()
            .manifest("firefox", "141.0", ApplicationProvenance::Foreign)
            .unwrap()
    }

    fn error(text: &str) -> String {
        ApplicationManifest::parse(text).unwrap_err()
    }

    #[test]
    fn a_manifest_round_trips_to_one_canonical_keyfile() {
        let parsed = ApplicationManifest::parse(VALID).unwrap();
        assert_eq!(parsed.name(), "firefox");
        assert_eq!(parsed.version(), "141.0");
        assert_eq!(parsed.alias(), Some("org.mozilla.firefox"));
        assert_eq!(parsed.runtime(), "freedesktop-sdk-24.08");
        assert_eq!(parsed.entry(), "/app/bin/firefox");
        assert_eq!(parsed.provenance(), ApplicationProvenance::Foreign);
        assert_eq!(parsed.environment().count(), 2);
        assert_eq!(parsed.to_keyfile(), VALID);
        assert_eq!(
            ApplicationManifest::parse(&parsed.to_keyfile()).unwrap(),
            parsed
        );
    }

    #[test]
    fn the_writer_sorts_environment_and_emits_no_empty_section() {
        let plain = base();
        assert!(!plain.to_keyfile().contains("[Environment]"));
        let authored_empty = format!("{}\n[Environment]\n", plain.to_keyfile());
        assert_eq!(
            ApplicationManifest::parse(&authored_empty)
                .unwrap()
                .to_keyfile(),
            plain.to_keyfile(),
            "a leniently parsed empty section canonicalizes away"
        );
        let with = plain
            .with_environment("Z_LAST", "z")
            .unwrap()
            .with_environment("A_FIRST", "a")
            .unwrap();
        let text = with.to_keyfile();
        assert!(text.contains("[Environment]\nA_FIRST=a\nZ_LAST=z\n"));
    }

    #[test]
    fn declaration_json_omits_the_final_recipes_answers() {
        let declaration = ApplicationDeclaration::new("runtime", "/app/bin/firefox")
            .unwrap()
            .with_alias("org.mozilla.firefox")
            .unwrap()
            .with_environment("TOKEN", "{root}")
            .unwrap();
        let json = declaration.to_json();
        assert_eq!(
            json.to_canonical(),
            r#"{"alias":"org.mozilla.firefox","entry":"/app/bin/firefox","environment":{"TOKEN":"{root}"},"runtime":"runtime"}"#
        );
        assert_eq!(
            ApplicationDeclaration::from_json(&json).unwrap(),
            declaration
        );
        assert!(json.get("name").is_none());
        assert!(json.get("version").is_none());
        assert!(json.get("provenance").is_none());
    }

    #[test]
    fn declaration_json_is_strict_and_typed() {
        for (json, reason) in [
            (Json::Null, "not a JSON object"),
            (Json::Obj(Vec::new()), "missing `runtime'"),
            (
                Json::Obj(vec![
                    ("runtime".into(), Json::Str("runtime".into())),
                    ("entry".into(), Json::Str("/app/bin/app".into())),
                    ("unknown".into(), Json::Str("x".into())),
                ]),
                "unknown application declaration key",
            ),
            (
                Json::Obj(vec![
                    ("runtime".into(), Json::Bool(true)),
                    ("entry".into(), Json::Str("/app/bin/app".into())),
                ]),
                "is not a string",
            ),
            (
                Json::Obj(vec![
                    ("runtime".into(), Json::Str("runtime".into())),
                    ("entry".into(), Json::Str("/app/bin/app".into())),
                    ("environment".into(), Json::Bool(true)),
                ]),
                "environment is not a JSON object",
            ),
        ] {
            let got = ApplicationDeclaration::from_json(&json).unwrap_err();
            assert!(got.contains(reason), "{got}");
        }
    }

    #[test]
    fn final_identity_and_provenance_bind_when_the_manifest_is_rendered() {
        let declaration = ApplicationDeclaration::new("runtime", "/app/bin/firefox").unwrap();
        let foreign = declaration
            .manifest("renamed", "2", ApplicationProvenance::Foreign)
            .unwrap();
        assert_eq!(foreign.name(), "renamed");
        assert_eq!(foreign.version(), "2");
        assert_eq!(foreign.provenance(), ApplicationProvenance::Foreign);
        assert!(foreign.to_keyfile().contains("provenance=foreign\n"));

        let source = declaration
            .manifest("source-app", "1", ApplicationProvenance::Source)
            .unwrap();
        assert_eq!(source.provenance(), ApplicationProvenance::Source);
        assert!(source.to_keyfile().contains("provenance=source\n"));
    }

    #[test]
    fn identity_names_have_one_exact_language_and_bound() {
        for valid in ["firefox", "A.b_c-d9", ".hidden", &"a".repeat(32)] {
            assert!(validate_application_name(valid).is_ok(), "{valid:?}");
        }
        for (invalid, reason) in [
            ("", "empty"),
            (&"a".repeat(33), "limit is 32"),
            ("-firefox", "may not begin"),
            (".", "may not be `.'"),
            ("fire..fox", "may not contain `..'"),
            ("fire/fox", "only ASCII"),
            ("fire fox", "only ASCII"),
            ("fírefox", "only ASCII"),
        ] {
            let got = validate_application_name(invalid).unwrap_err();
            assert!(got.contains(reason), "{invalid:?}: {got}");
        }
    }

    #[test]
    fn every_required_declaration_is_required() {
        for (line, key) in [
            ("name=firefox\n", "name"),
            ("version=141.0\n", "version"),
            ("runtime=freedesktop-sdk-24.08\n", "runtime"),
            ("entry=/app/bin/firefox\n", "entry"),
            ("provenance=foreign\n", "provenance"),
        ] {
            let text = VALID.replacen(line, "", 1);
            let got = error(&text);
            assert!(got.contains(&format!("missing `{key}'")), "{key}: {got}");
        }
    }

    #[test]
    fn malformed_structure_unknown_intent_and_duplicates_are_refused() {
        for (text, reason) in [
            ("", "empty"),
            ("name=firefox", "trailing newline"),
            ("name=firefox\r\n", "carriage return"),
            ("name=fire\0fox\n", "NUL"),
            ("not a key\n", "expected key=value"),
            ("=firefox\n", "empty key"),
            ("[Permissions]\n", "unknown section"),
            ("[Environment]\n[Environment]\n", "duplicate [Environment]"),
            ("name=firefox\nname=other\n", "duplicate key"),
            ("unknown=value\n", "unknown declaration key"),
            ("[Environment]\nA=1\nA=2\n", "duplicate environment key"),
        ] {
            let got = error(text);
            assert!(got.contains(reason), "{text:?}: {got}");
        }
    }

    #[test]
    fn every_typed_value_is_validated_before_it_can_be_rendered() {
        let declaration = ApplicationDeclaration::new("runtime", "/app/bin/firefox").unwrap();
        assert!(declaration
            .manifest("firefox", "", ApplicationProvenance::Source)
            .unwrap_err()
            .contains("version is empty"));
        assert!(ApplicationDeclaration::new("-runtime", "/app/bin/firefox")
            .unwrap_err()
            .contains("runtime"));
        for entry in [
            "bin/firefox",
            "/usr/bin/firefox",
            "/app/../usr/bin/x",
            "/app/bin//x",
        ] {
            let got = ApplicationDeclaration::new("runtime", entry).unwrap_err();
            assert!(got.contains("entry"), "{entry:?}: {got}");
        }
        for alias in [
            "firefox",
            "org..firefox",
            "org.7zip.app",
            "org.mozilla.fire/fox",
        ] {
            let got = ApplicationDeclaration::new("runtime", "/app/bin/firefox")
                .unwrap()
                .with_alias(alias)
                .unwrap_err();
            assert!(got.contains("alias"), "{alias:?}: {got}");
        }
        assert!(ApplicationDeclaration::new("runtime", "/app/bin/firefox")
            .unwrap()
            .with_alias("org.mozilla.firefox")
            .unwrap()
            .with_alias("org.mozilla.other")
            .unwrap_err()
            .contains("duplicate"));
        for name in ["", "9NAME", "BAD-NAME"] {
            let got = ApplicationDeclaration::new("runtime", "/app/bin/firefox")
                .unwrap()
                .with_environment(name, "value")
                .unwrap_err();
            assert!(got.contains("environment name"), "{name:?}: {got}");
        }
        assert!(ApplicationDeclaration::new("runtime", "/app/bin/firefox")
            .unwrap()
            .with_environment("NAME", "line\nvalue")
            .unwrap_err()
            .contains("control character"));
    }

    #[test]
    fn parser_errors_name_the_authored_line() {
        let got = error(
            "name=firefox\nversion=1\nruntime=runtime\nentry=/usr/bin/x\nprovenance=source\n",
        );
        assert!(got.starts_with("application manifest line 4:"), "{got}");
        assert!(got.contains("below `/app/'"), "{got}");
    }

    #[test]
    fn comments_and_layout_whitespace_canonicalize_away() {
        let text = "# package declaration\n name = firefox \nversion = 141.0\nruntime=freedesktop-sdk-24.08\nentry = /app/bin/firefox\nprovenance = foreign\n\n [Environment] \n TZ = UTC \n";
        let parsed = ApplicationManifest::parse(text).unwrap();
        assert_eq!(
            parsed.to_keyfile(),
            "name=firefox\nversion=141.0\nruntime=freedesktop-sdk-24.08\nentry=/app/bin/firefox\nprovenance=foreign\n\n[Environment]\nTZ=UTC\n"
        );
    }

    #[test]
    fn the_manifest_and_environment_are_bounded() {
        let oversized = format!("#{}\n", "x".repeat(MAX_MANIFEST_BYTES));
        assert!(error(&oversized).contains("limit is 16384"));
        let mut manifest = base();
        for index in 0..MAX_ENVIRONMENT_ENTRIES {
            manifest = manifest
                .with_environment(&format!("V{index}"), "x")
                .unwrap();
        }
        assert!(manifest
            .with_environment("ONE_TOO_MANY", "x")
            .unwrap_err()
            .contains("at most 128"));

        let large = "x".repeat(MAX_ENVIRONMENT_VALUE_BYTES);
        let mut bounded = base();
        let mut stopped = false;
        for index in 0..MAX_ENVIRONMENT_ENTRIES {
            match bounded
                .clone()
                .with_environment(&format!("L{index}"), &large)
            {
                Ok(next) => bounded = next,
                Err(got) => {
                    assert!(got.contains("limit is 16384"), "{got}");
                    stopped = true;
                    break;
                }
            }
        }
        assert!(stopped, "aggregate rendering must reach the manifest bound");
    }

    #[test]
    fn provenance_has_no_fail_open_spelling() {
        for value in ["", "Foreign", "fore ign", "source-built", "unknown"] {
            let text = VALID.replace("provenance=foreign", &format!("provenance={value}"));
            let got = error(&text);
            assert!(got.contains("provenance"), "{value:?}: {got}");
        }
    }

    #[test]
    fn loader_environment_is_content_until_spec_policy_but_td_is_reserved() {
        let declaration = ApplicationDeclaration::new("runtime", "/app/bin/firefox")
            .unwrap()
            .with_environment("LD_PRELOAD", "/app/lib/instrument.so")
            .unwrap();
        assert_eq!(
            declaration.environment().next(),
            Some(("LD_PRELOAD", "/app/lib/instrument.so"))
        );
        let error = ApplicationDeclaration::new("runtime", "/app/bin/firefox")
            .unwrap()
            .with_environment("TD_PRIVATE", "value")
            .unwrap_err();
        assert!(error.contains("reserved by td"), "{error}");
    }
}
