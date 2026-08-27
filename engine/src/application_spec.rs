//! The immutable jail input compiled beside an application manifest.
//!
//! The manifest keeps recipe-authored identity; this file binds it to the full
//! runtime store path and to typed permission defaults. `td-jail` later reads
//! only this closed, canonical format rather than interpreting package bytes.

use crate::application::{
    validate_application_identity, validate_entry, validate_environment_name,
    validate_environment_value, ApplicationManifest,
};
use crate::permissions::PermissionPolicy;
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_APPLICATION_SPEC_BYTES: usize = 48 * 1024;
const MAX_RUNTIME_PATH_BYTES: usize = 4096;
pub const MAX_SPEC_ENVIRONMENT_ENTRIES: usize = 256;
pub const APPLICATION_UID: u32 = 1000;
const FIREFOX_LIBRARY_PATHS: &[&str] = &["/app/lib", "/app/lib/firefox"];
const FIREFOX_OPTIONAL_TARGETS: &[&str] = &["/app/share/runtime/langpack"];
const FIREFOX_OPTIONAL_LINKS: usize = 102;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DynamicApplicationPolicy {
    library_paths: &'static [&'static str],
    optional_targets: &'static [&'static str],
    optional_links: usize,
}

impl DynamicApplicationPolicy {
    pub fn library_paths(self) -> &'static [&'static str] {
        self.library_paths
    }

    pub fn optional_targets(self) -> &'static [&'static str] {
        self.optional_targets
    }

    pub fn optional_links(self) -> usize {
        self.optional_links
    }
}

pub fn dynamic_application_policy(
    name: &str,
    runtime: &str,
) -> Option<DynamicApplicationPolicy> {
    match (name, runtime) {
        ("firefox", "freedesktop-platform-25-08") => Some(DynamicApplicationPolicy {
            library_paths: FIREFOX_LIBRARY_PATHS,
            optional_targets: FIREFOX_OPTIONAL_TARGETS,
            optional_links: FIREFOX_OPTIONAL_LINKS,
        }),
        _ => None,
    }
}

/// Variables `td-jail` holds a spec to EXACTLY, and which a manifest may
/// therefore not set.
///
/// Without this the loop below would let a manifest overwrite one after the
/// compiler had inserted it, producing a spec that compiles here and is refused
/// at launch — a package that builds and cannot start, diagnosed at the far end
/// from the thing that caused it. Refusing at compile time puts the diagnostic
/// where the packager can act on it, and is what lets the two sides be
/// described as agreeing by construction rather than by coincidence.
///
/// This is the same rule as the `LD_*` refusal below for a different reason,
/// and `TD_*` is refused a layer down by the name grammar.
///
/// "Pins" is not all one thing: `authority.rs` holds four of these to an exact
/// VALUE and requires `FLATPAK_ID` to be present and non-empty, its value being
/// per-application. Both are refusals a manifest cannot talk its way past, which
/// is what puts them on one list; adding a name to either check there means
/// adding it here.
const PINNED_ENVIRONMENT: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "FLATPAK_ID",
    "HOME",
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
];

const BASE_ENVIRONMENT: &[(&str, &str)] = &[
    ("GDK_BACKEND", "wayland"),
    ("GTK_A11Y", "none"),
    ("HOME", "/home/td"),
    ("LOGNAME", "td"),
    ("PATH", "/app/bin:/usr/bin"),
    ("SHELL", "/bin/sh"),
    ("USER", "td"),
    ("WAYLAND_DISPLAY", "wayland-0"),
    ("XDG_CACHE_HOME", "/home/td/.cache"),
    ("XDG_CONFIG_DIRS", "/app/etc/xdg:/etc/xdg"),
    ("XDG_CONFIG_HOME", "/home/td/.config"),
    ("XDG_CURRENT_DESKTOP", "td"),
    ("XDG_DATA_DIRS", "/app/share:/usr/share"),
    ("XDG_DATA_HOME", "/home/td/.local/share"),
    ("XDG_SESSION_DESKTOP", "td"),
    ("XDG_SESSION_TYPE", "wayland"),
    ("XDG_STATE_HOME", "/home/td/.local/state"),
    ("container", "flatpak"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationSpec {
    name: String,
    runtime: String,
    entry: String,
    environment: BTreeMap<String, String>,
    permissions: PermissionPolicy,
}

impl ApplicationSpec {
    /// Compile validated package metadata against the runtime path selected by
    /// the derivation lock. Runtime-specific environment policy is closed: a
    /// new runtime must gain a reviewed table entry before any spec can name it.
    pub fn compile(
        manifest: &ApplicationManifest,
        runtime_path: &str,
        permissions: PermissionPolicy,
    ) -> Result<ApplicationSpec, String> {
        validate_runtime_path(runtime_path)?;
        let mut environment: BTreeMap<String, String> = BASE_ENVIRONMENT
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        let runtime_directory = format!("/run/user/{APPLICATION_UID}");
        environment.insert(
            "DBUS_SESSION_BUS_ADDRESS".into(),
            format!("unix:path={runtime_directory}/bus"),
        );
        environment.insert("XDG_RUNTIME_DIR".into(), runtime_directory);
        environment.insert(
            "FLATPAK_ID".into(),
            manifest.alias().unwrap_or(manifest.name()).to_string(),
        );
        for (name, value) in manifest.environment() {
            if name.starts_with("LD_") {
                return Err(format!(
                    "application environment {name:?} controls the dynamic loader and is not allowed in a jail spec"
                ));
            }
            if PINNED_ENVIRONMENT.contains(&name) {
                return Err(format!(
                    "application environment {name:?} is fixed by the jail contract and cannot be set by a manifest"
                ));
            }
            environment.insert(name.to_string(), value.to_string());
        }
        match manifest.runtime() {
            // Fully static payload: no runtime-major rendering override exists.
            "empty-runtime" => {}
            "freedesktop-platform-25-08" => {
                if manifest
                    .environment()
                    .any(|(name, _)| name == "LIBGL_ALWAYS_SOFTWARE")
                {
                    return Err(
                        "application environment \"LIBGL_ALWAYS_SOFTWARE\" is fixed by the \
                         Freedesktop 25.08 runtime policy"
                            .into(),
                    );
                }
                environment.insert("LIBGL_ALWAYS_SOFTWARE".into(), "1".into());
            }
            runtime => {
                return Err(format!(
                    "application runtime {runtime:?} has no compiled environment policy"
                ));
            }
        }
        if let Some(policy) = dynamic_application_policy(manifest.name(), manifest.runtime()) {
            environment.insert("LD_LIBRARY_PATH".into(), policy.library_paths().join(":"));
        }
        let spec = ApplicationSpec {
            name: manifest.name().to_string(),
            runtime: runtime_path.to_string(),
            entry: manifest.entry().to_string(),
            environment,
            permissions,
        };
        spec.validate()?;
        Ok(spec)
    }

    /// Parse only the canonical builder-owned representation. There is no
    /// authored-spec compatibility grammar: a noncanonical file is not one the
    /// compiler emitted and is refused.
    pub fn parse(text: &str) -> Result<ApplicationSpec, String> {
        validate_file_shape(text)?;
        let mut format = None;
        let mut name = None;
        let mut runtime = None;
        let mut entry = None;
        let mut environment = BTreeMap::new();
        let mut environment_seen = false;
        let mut permission_sections = BTreeSet::new();
        let mut permission_text = String::new();
        let mut permission_has_cpu_max = false;
        let mut section: Option<&str> = None;

        for (index, raw) in text.lines().enumerate() {
            let line_number = index + 1;
            if raw.is_empty() {
                continue;
            }
            if raw.starts_with('[') {
                if !raw.ends_with(']') || raw.len() < 3 {
                    return Err(at_line(line_number, "malformed section header"));
                }
                let section_name = raw
                    .get(1..raw.len().saturating_sub(1))
                    .ok_or_else(|| at_line(line_number, "malformed section header"))?;
                if section_name == "Environment" {
                    if environment_seen {
                        return Err(at_line(line_number, "duplicate [Environment] section"));
                    }
                    environment_seen = true;
                } else if !permission_sections.insert(section_name.to_string()) {
                    return Err(at_line(
                        line_number,
                        &format!("duplicate [{section_name}] section"),
                    ));
                } else {
                    permission_text.push('\n');
                    permission_text.push('[');
                    permission_text.push_str(section_name);
                    permission_text.push_str("]\n");
                }
                section = Some(section_name);
                continue;
            }
            let Some((key, value)) = raw.split_once('=') else {
                return Err(at_line(line_number, "entry has no `=' delimiter"));
            };
            match section {
                None => match key {
                    "format" => set_once(&mut format, value, "format", line_number)?,
                    "name" => set_once(&mut name, value, "name", line_number)?,
                    "runtime" => set_once(&mut runtime, value, "runtime", line_number)?,
                    "entry" => set_once(&mut entry, value, "entry", line_number)?,
                    _ => {
                        return Err(at_line(
                            line_number,
                            &format!("unknown application spec key {key:?}"),
                        ));
                    }
                },
                Some("Environment") => {
                    validate_environment_name(key)
                        .map_err(|reason| at_line(line_number, &reason))?;
                    validate_environment_value(value)
                        .map_err(|reason| at_line(line_number, &reason))?;
                    if environment.len() >= MAX_SPEC_ENVIRONMENT_ENTRIES {
                        return Err(at_line(
                            line_number,
                            &format!(
                                "an application spec may carry at most {MAX_SPEC_ENVIRONMENT_ENTRIES} environment entries"
                            ),
                        ));
                    }
                    if environment
                        .insert(key.to_string(), value.to_string())
                        .is_some()
                    {
                        return Err(at_line(
                            line_number,
                            &format!("duplicate environment key {key:?}"),
                        ));
                    }
                }
                Some(permission_section) => {
                    if !matches!(
                        permission_section,
                        "Context" | "Filesystem" | "Session Bus Policy" | "Resources"
                    ) {
                        return Err(at_line(
                            line_number,
                            &format!("unknown application spec section [{permission_section}]"),
                        ));
                    }
                    if permission_section == "Resources" && key == "cpu-max" {
                        permission_has_cpu_max = true;
                    }
                    permission_text.push_str(raw);
                    permission_text.push('\n');
                }
            }
        }

        if format.as_deref() != Some("1") {
            return Err("application spec requires `format=1'".into());
        }
        let name = name.ok_or("application spec is missing `name'")?;
        let runtime = runtime.ok_or("application spec is missing `runtime'")?;
        let entry = entry.ok_or("application spec is missing `entry'")?;
        permission_text.insert_str(
            0,
            if permission_has_cpu_max {
                "format=2\n"
            } else {
                "format=1\n"
            },
        );
        let permissions = PermissionPolicy::parse(&permission_text)
            .map_err(|reason| format!("application spec permissions: {reason}"))?;
        let spec = ApplicationSpec {
            name,
            runtime,
            entry,
            environment,
            permissions,
        };
        spec.validate()?;
        if spec.to_keyfile() != text {
            return Err("application spec is not canonical".into());
        }
        Ok(spec)
    }

    pub fn to_keyfile(&self) -> String {
        let mut out = String::from("format=1\n");
        push_key(&mut out, "name", &self.name);
        push_key(&mut out, "runtime", &self.runtime);
        push_key(&mut out, "entry", &self.entry);
        if !self.environment.is_empty() {
            out.push_str("\n[Environment]\n");
            for (name, value) in &self.environment {
                push_key(&mut out, name, value);
            }
        }
        let permissions = self.permissions.to_keyfile();
        if let Some(tail) = permissions
            .strip_prefix("format=1\n")
            .or_else(|| permissions.strip_prefix("format=2\n"))
        {
            out.push_str(tail);
        }
        out
    }

    pub fn name(&self) -> &str {
        &self.name
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

    pub fn permissions(&self) -> &PermissionPolicy {
        &self.permissions
    }

    fn validate(&self) -> Result<(), String> {
        validate_application_identity(&self.name)?;
        validate_runtime_path(&self.runtime)?;
        validate_entry(&self.entry)?;
        if self.environment.len() > MAX_SPEC_ENVIRONMENT_ENTRIES {
            return Err(format!(
                "an application spec may carry at most {MAX_SPEC_ENVIRONMENT_ENTRIES} environment entries"
            ));
        }
        for (name, value) in &self.environment {
            validate_environment_name(name)?;
            validate_environment_value(value)?;
            if name.starts_with("LD_") && name != "LD_LIBRARY_PATH" {
                return Err(format!(
                    "application environment {name:?} controls the dynamic loader and is not allowed in a jail spec"
                ));
            }
        }
        let loader_policy = runtime_recipe_name(&self.runtime)
            .and_then(|runtime| dynamic_application_policy(&self.name, runtime));
        match (
            loader_policy,
            self.environment.get("LD_LIBRARY_PATH").map(String::as_str),
        ) {
            (Some(policy), Some(value)) if value == policy.library_paths().join(":") => {}
            (Some(_), _) => {
                return Err(
                    "application spec does not carry its exact reviewed loader path".into(),
                );
            }
            (None, None) => {}
            (None, Some(_)) => {
                return Err(
                    "application spec carries an unreviewed LD_LIBRARY_PATH policy".into(),
                );
            }
        }
        self.permissions.resources().complete_or_default()?;
        let size = self.to_keyfile().len();
        if size > MAX_APPLICATION_SPEC_BYTES {
            return Err(format!(
                "application spec would be {size} bytes; the limit is {MAX_APPLICATION_SPEC_BYTES}"
            ));
        }
        Ok(())
    }
}

fn runtime_recipe_name(path: &str) -> Option<&str> {
    let package = runtime_store_name(path).ok()?;
    ["freedesktop-platform-25-08", "empty-runtime"]
        .into_iter()
        .find(|name| {
            package == *name
                || package
                    .strip_prefix(*name)
                    .is_some_and(|suffix| suffix.starts_with('-'))
        })
}

fn runtime_store_name(path: &str) -> Result<&str, String> {
    const STORE_HASH_ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";

    let basename = path
        .strip_prefix("/td/store/")
        .ok_or("application runtime path must be a full `/td/store' path")?;
    let digest = basename
        .as_bytes()
        .get(..32)
        .ok_or("application runtime path has no 32-character store digest")?;
    if !digest
        .iter()
        .all(|byte| STORE_HASH_ALPHABET.contains(byte))
        || basename.as_bytes().get(32) != Some(&b'-')
    {
        return Err("application runtime path has a malformed store basename".into());
    }
    let name = basename
        .get(33..)
        .ok_or("application runtime path has a malformed store name")?;
    if name.is_empty() || name.contains('/') || matches!(name, "." | "..") {
        return Err("application runtime path is not one canonical store child".into());
    }
    Ok(name)
}

fn validate_runtime_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("application runtime path is empty".into());
    }
    if path.len() > MAX_RUNTIME_PATH_BYTES {
        return Err(format!(
            "application runtime path is {} bytes; the limit is {MAX_RUNTIME_PATH_BYTES}",
            path.len()
        ));
    }
    if path.bytes().any(|byte| byte.is_ascii_control()) {
        return Err("application runtime path contains a control byte".into());
    }
    runtime_store_name(path)?;
    Ok(())
}

fn validate_file_shape(text: &str) -> Result<(), String> {
    if text.len() > MAX_APPLICATION_SPEC_BYTES {
        return Err(format!(
            "application spec is {} bytes; the limit is {MAX_APPLICATION_SPEC_BYTES}",
            text.len()
        ));
    }
    if text.is_empty() {
        return Err("application spec is empty".into());
    }
    if !text.ends_with('\n') {
        return Err("application spec lacks a trailing newline".into());
    }
    if text.contains('\r') {
        return Err("application spec contains a carriage return".into());
    }
    if text.contains('\0') {
        return Err("application spec contains a NUL byte".into());
    }
    Ok(())
}

fn set_once(slot: &mut Option<String>, value: &str, key: &str, line: usize) -> Result<(), String> {
    if slot.is_some() {
        return Err(at_line(
            line,
            &format!("duplicate application spec key {key:?}"),
        ));
    }
    *slot = Some(value.to_string());
    Ok(())
}

fn push_key(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push('=');
    out.push_str(value);
    out.push('\n');
}

fn at_line(line: usize, reason: &str) -> String {
    format!("application spec line {line}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{ApplicationDeclaration, ApplicationProvenance};
    use crate::permissions::{BusAccess, FilesystemAccess, PermissionSocket};

    fn manifest() -> ApplicationManifest {
        ApplicationDeclaration::new("empty-runtime", "/app/bin/rg")
            .unwrap()
            .with_alias("org.td.ripgrep")
            .unwrap()
            .manifest("ripgrep-seed", "15.2.0", ApplicationProvenance::Foreign)
            .unwrap()
    }

    #[test]
    fn compiler_binds_the_full_runtime_and_effective_environment() {
        let runtime = "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1";
        let spec = ApplicationSpec::compile(&manifest(), runtime, PermissionPolicy::new()).unwrap();
        assert_eq!(spec.name(), "ripgrep-seed");
        assert_eq!(spec.runtime(), runtime);
        assert_eq!(spec.entry(), "/app/bin/rg");
        let environment: BTreeMap<&str, &str> = spec.environment().collect();
        assert_eq!(environment.get("PATH"), Some(&"/app/bin:/usr/bin"));
        assert_eq!(environment.get("FLATPAK_ID"), Some(&"org.td.ripgrep"));
        assert_eq!(
            environment.get("DBUS_SESSION_BUS_ADDRESS"),
            Some(&format!("unix:path=/run/user/{APPLICATION_UID}/bus").as_str())
        );
        assert_eq!(
            environment.get("XDG_RUNTIME_DIR"),
            Some(&format!("/run/user/{APPLICATION_UID}").as_str())
        );
        assert!(!spec.to_keyfile().contains("provenance="));
        assert_eq!(ApplicationSpec::parse(&spec.to_keyfile()).unwrap(), spec);
    }

    #[test]
    fn permissions_are_embedded_without_a_second_format_key() {
        let policy = PermissionPolicy::new()
            .with_network()
            .unwrap()
            .with_socket(PermissionSocket::Wayland)
            .unwrap()
            .with_filesystem("xdg-download", FilesystemAccess::ReadOnly, false)
            .unwrap()
            .with_session_bus("org.example.Search", BusAccess::Talk)
            .unwrap()
            .with_memory_high(48 * 1024 * 1024)
            .unwrap()
            .with_memory_max(64 * 1024 * 1024)
            .unwrap()
            .with_pids_max(32)
            .unwrap()
            .with_cpu_max(50_000, 100_000)
            .unwrap();
        let spec = ApplicationSpec::compile(
            &manifest(),
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
            policy.clone(),
        )
        .unwrap();
        let text = spec.to_keyfile();
        assert_eq!(text.matches("format=1\n").count(), 1);
        assert!(!text.contains("format=2\n"));
        assert!(text.contains("[Context]\nshared=network\nsockets=wayland\n"));
        assert!(text.contains("[Filesystem]\nxdg-download=ro\n"));
        assert!(text.contains("cpu-max=50000 100000\n"));
        assert_eq!(
            ApplicationSpec::parse(&text).unwrap().permissions(),
            &policy
        );
    }

    #[test]
    fn package_loader_controls_are_refused_at_compilation() {
        for name in ["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH", "LD_DEBUG"] {
            let declaration = ApplicationDeclaration::new("empty-runtime", "/app/bin/rg")
                .unwrap()
                .with_environment(name, "/app/value")
                .unwrap();
            let manifest = declaration
                .manifest("ripgrep-seed", "15.2.0", ApplicationProvenance::Foreign)
                .unwrap();
            let error = ApplicationSpec::compile(
                &manifest,
                "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
                PermissionPolicy::new(),
            )
            .unwrap_err();
            assert!(error.contains("dynamic loader"), "{name}: {error}");
        }
    }

    /// A manifest may not set what the jail pins.
    ///
    /// The failure this prevents is not a security one — td-jail refuses the
    /// spec either way — it is a package that compiles and then cannot start,
    /// reported by the sandbox at launch rather than by the compiler that had
    /// the manifest in its hands.
    #[test]
    fn package_overrides_of_jail_pinned_variables_are_refused_at_compilation() {
        for name in [
            "DBUS_SESSION_BUS_ADDRESS",
            "FLATPAK_ID",
            "HOME",
            "WAYLAND_DISPLAY",
            "XDG_RUNTIME_DIR",
        ] {
            let declaration = ApplicationDeclaration::new("empty-runtime", "/app/bin/rg")
                .unwrap()
                .with_environment(name, "/app/value")
                .unwrap();
            let manifest = declaration
                .manifest("ripgrep-seed", "15.2.0", ApplicationProvenance::Foreign)
                .unwrap();
            let error = ApplicationSpec::compile(
                &manifest,
                "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
                PermissionPolicy::new(),
            )
            .unwrap_err();
            assert!(error.contains("fixed by the jail contract"), "{name}: {error}");
        }
    }

    #[test]
    fn compiler_refuses_an_unreviewed_runtime_policy() {
        let manifest = ApplicationDeclaration::new("future-runtime", "/app/bin/app")
            .unwrap()
            .manifest("app", "1", ApplicationProvenance::Source)
            .unwrap();
        let error = ApplicationSpec::compile(
            &manifest,
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-future-runtime-1",
            PermissionPolicy::new(),
        )
        .unwrap_err();
        assert!(error.contains("no compiled environment policy"), "{error}");
    }

    #[test]
    fn freedesktop_25_08_policy_forces_software_mesa() {
        let declaration =
            ApplicationDeclaration::new("freedesktop-platform-25-08", "/app/bin/firefox")
                .unwrap()
                .with_environment("MOZ_DISABLE_AUTO_SAFE_MODE", "1")
                .unwrap()
                .with_environment("MOZ_ENABLE_WAYLAND", "1")
                .unwrap();
        let manifest = declaration
            .manifest("firefox", "154.0", ApplicationProvenance::Foreign)
            .unwrap();
        let runtime =
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-freedesktop-platform-25-08-25.08";
        let spec =
            ApplicationSpec::compile(&manifest, runtime, PermissionPolicy::new()).unwrap();
        let environment = spec.environment().collect::<BTreeMap<_, _>>();
        assert_eq!(environment.get("LIBGL_ALWAYS_SOFTWARE"), Some(&"1"));
        assert_eq!(
            environment.get("LD_LIBRARY_PATH"),
            Some(&"/app/lib:/app/lib/firefox")
        );
        assert_eq!(environment.get("MOZ_DISABLE_AUTO_SAFE_MODE"), Some(&"1"));
        assert_eq!(environment.get("MOZ_ENABLE_WAYLAND"), Some(&"1"));
        let text = spec.to_keyfile();
        assert_eq!(ApplicationSpec::parse(&text).unwrap(), spec);
        assert!(ApplicationSpec::parse(&text.replace(
            "LD_LIBRARY_PATH=/app/lib:/app/lib/firefox",
            "LD_LIBRARY_PATH=/app/lib"
        ))
        .is_err());
        let malformed_store = text.replacen("runtime=/td/store/0", "runtime=/td/store/e", 1);
        let error = ApplicationSpec::parse(&malformed_store).unwrap_err();
        assert!(error.contains("malformed store basename"), "{error}");

        let overridden =
            ApplicationDeclaration::new("freedesktop-platform-25-08", "/app/bin/firefox")
                .unwrap()
                .with_environment("LIBGL_ALWAYS_SOFTWARE", "0")
                .unwrap()
                .manifest("firefox", "154.0", ApplicationProvenance::Foreign)
                .unwrap();
        let error =
            ApplicationSpec::compile(&overridden, runtime, PermissionPolicy::new()).unwrap_err();
        assert!(error.contains("Freedesktop 25.08 runtime policy"), "{error}");
    }

    #[test]
    fn compiler_refuses_incomplete_or_unaligned_resource_policy() {
        let runtime = "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1";
        let partial = PermissionPolicy::new().with_pids_max(32).unwrap();
        let error = ApplicationSpec::compile(&manifest(), runtime, partial).unwrap_err();
        assert!(error.contains("must set memory-high, memory-max and pids-max together"));

        let unaligned = PermissionPolicy::new()
            .with_memory_high(48 * 1024 * 1024 + 1)
            .unwrap()
            .with_memory_max(64 * 1024 * 1024)
            .unwrap()
            .with_pids_max(32)
            .unwrap();
        let error = ApplicationSpec::compile(&manifest(), runtime, unaligned).unwrap_err();
        assert!(error.contains("must be aligned to the 4096-byte target page size"));
    }

    #[test]
    fn parser_refuses_noncanonical_and_malformed_specs() {
        let spec = ApplicationSpec::compile(
            &manifest(),
            "/td/store/0123456789abcdfghijklmnpqrsvwxyz-empty-runtime-1",
            PermissionPolicy::new(),
        )
        .unwrap();
        let text = spec.to_keyfile();
        assert!(ApplicationSpec::parse(&text.replace("format=1\n", "format = 1\n")).is_err());
        assert!(ApplicationSpec::parse(text.trim_end()).is_err());
        assert!(ApplicationSpec::parse(
            &text.replace("runtime=/td/store/", "runtime=/td/store/../")
        )
        .is_err());
        assert!(ApplicationSpec::parse(
            &text.replace("runtime=/td/store/", "runtime=/gnu/store/")
        )
        .is_err());
        assert!(ApplicationSpec::parse(
            &text.replace("-empty-runtime-1", "/nested-runtime")
        )
        .is_err());
        assert!(ApplicationSpec::parse(&format!("{text}[Unknown]\nx=y\n")).is_err());
    }
}
