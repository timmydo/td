//! Canonical application launcher metadata and the two image tables derived from it.

use crate::application::{
    validate_application_identity, ApplicationManifest, RESERVED_APPLICATION_NAMES,
};
use crate::json::Json;
use std::collections::BTreeSet;

pub const MAX_DISPLAY_NAME_BYTES: usize = 128;
pub const MAX_SEARCH_TERM_BYTES: usize = 64;
pub const MAX_SEARCH_TERMS: usize = 32;
pub const MAX_LAUNCHER_EXPORT_BYTES: usize = 4096;
pub const MAX_APPLICATION_TABLE_BYTES: usize = 1024 * 1024;
pub const MAX_APPLICATIONS: usize = 256;
pub const MAX_PACKAGE_PATH_BYTES: usize = 4096;
/// The longest window app id a table row carries: the compositor's bound
/// on an `xdg_toplevel.set_app_id` it matches.
pub const MAX_APP_ID_BYTES: usize = 128;
/// What `compileApplicationTables` writes, carried in its step so a change
/// to the tables re-keys the image that holds them (builder/src/store.rs,
/// `BUILDER_ABI`): the launcher table with its app id column.
pub const TABLES_FORMAT: &str = "launcher-app-id-v1";
pub const RESERVED_LAUNCHER_NAMES: &[&str] = RESERVED_APPLICATION_NAMES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherDeclaration {
    display_name: String,
    search_terms: Vec<String>,
}

impl LauncherDeclaration {
    pub fn new(display_name: &str, search_terms: &[&str]) -> Result<Self, String> {
        validate_display_name(display_name)?;
        validate_search_terms(search_terms.iter().copied())?;
        Ok(Self {
            display_name: display_name.to_string(),
            search_terms: search_terms
                .iter()
                .map(|term| (*term).to_string())
                .collect(),
        })
    }

    pub fn from_json(value: &Json) -> Result<Self, String> {
        let Json::Obj(object) = value else {
            return Err("application launcher declaration is not a JSON object".into());
        };
        let mut display_name = None;
        let mut search_terms = None;
        for (key, value) in object {
            match key.as_str() {
                "displayName" if display_name.is_none() => {
                    display_name = Some(
                        value
                            .as_str()
                            .ok_or("application launcher displayName is not a string")?,
                    );
                }
                "searchTerms" if search_terms.is_none() => {
                    let values = value
                        .as_arr()
                        .ok_or("application launcher searchTerms is not an array")?;
                    let terms = values
                        .iter()
                        .map(|term| {
                            term.as_str().ok_or_else(|| {
                                "application launcher searchTerms contains a non-string".to_string()
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    search_terms = Some(terms);
                }
                "displayName" | "searchTerms" => {
                    return Err(format!(
                        "duplicate application launcher declaration key {key:?}"
                    ));
                }
                _ => {
                    return Err(format!(
                        "unknown application launcher declaration key {key:?}"
                    ));
                }
            }
        }
        let display_name = display_name.ok_or("application launcher is missing displayName")?;
        let search_terms = search_terms.ok_or("application launcher is missing searchTerms")?;
        Self::new(display_name, &search_terms)
    }

    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("displayName".into(), Json::Str(self.display_name.clone())),
            (
                "searchTerms".into(),
                Json::Arr(
                    self.search_terms
                        .iter()
                        .map(|term| Json::Str(term.clone()))
                        .collect(),
                ),
            ),
        ])
    }

    pub fn bind(&self, name: &str) -> Result<LauncherExport, String> {
        LauncherExport::new(name, &self.display_name, &self.search_terms)
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn search_terms(&self) -> impl Iterator<Item = &str> {
        self.search_terms.iter().map(String::as_str)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherExport {
    name: String,
    display_name: String,
    search_terms: Vec<String>,
}

impl LauncherExport {
    pub fn new(name: &str, display_name: &str, search_terms: &[String]) -> Result<Self, String> {
        validate_application_identity(name)?;
        validate_display_name(display_name)?;
        validate_search_terms(search_terms.iter().map(String::as_str))?;
        let export = Self {
            name: name.to_string(),
            display_name: display_name.to_string(),
            search_terms: search_terms.to_vec(),
        };
        ensure_size(
            "application launcher export",
            &export.to_tsv(),
            MAX_LAUNCHER_EXPORT_BYTES,
        )?;
        Ok(export)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        ensure_text(
            "application launcher export",
            text,
            MAX_LAUNCHER_EXPORT_BYTES,
        )?;
        let line = text
            .strip_suffix('\n')
            .ok_or("application launcher export lacks a trailing newline")?;
        if line.contains('\n') {
            return Err("application launcher export must contain exactly one row".into());
        }
        let fields = split_fields(line, 3, "application launcher export")?;
        let name = fields
            .first()
            .ok_or("application launcher export is missing its name")?;
        let display_name = fields
            .get(1)
            .ok_or("application launcher export is missing its display name")?;
        let search = fields
            .get(2)
            .ok_or("application launcher export is missing its search terms")?;
        let terms: Vec<String> = if search.is_empty() {
            Vec::new()
        } else {
            search.split(' ').map(str::to_string).collect()
        };
        let export = Self::new(name, display_name, &terms)?;
        if export.to_tsv() != text {
            return Err("application launcher export is not canonical".into());
        }
        Ok(export)
    }

    pub fn to_tsv(&self) -> String {
        format!(
            "{}\t{}\t{}\n",
            self.name,
            self.display_name,
            self.search_terms.join(" ")
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn search_terms(&self) -> impl Iterator<Item = &str> {
        self.search_terms.iter().map(String::as_str)
    }
}

/// One `/etc/td-launcher.tsv` row: the package's export and the app id its
/// windows announce, which the compositor matches a toplevel by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherRow {
    export: LauncherExport,
    app_id: String,
}

impl LauncherRow {
    pub fn new(export: LauncherExport, app_id: &str) -> Result<Self, String> {
        validate_app_id(app_id)?;
        Ok(Self {
            export,
            app_id: app_id.to_string(),
        })
    }

    /// The row for a package's export and manifest, whose identities the
    /// caller has matched: the app id is the alias, the foreign id §A keeps
    /// for matching a toplevel, or for a td-built application without one
    /// its entry's file name, which a td-ui program gives its window.
    pub fn for_manifest(
        export: LauncherExport,
        manifest: &ApplicationManifest,
    ) -> Result<Self, String> {
        let entry = manifest.entry();
        let app_id = manifest
            .alias()
            .unwrap_or_else(|| entry.rsplit('/').next().unwrap_or(entry));
        Self::new(export, app_id)
    }

    fn parse(line: &str) -> Result<Self, String> {
        let (export, app_id) = line
            .rsplit_once('\t')
            .ok_or("launcher table row has no app id")?;
        Self::new(LauncherExport::parse(&format!("{export}\n"))?, app_id)
    }

    pub fn to_tsv(&self) -> String {
        let export = self.export.to_tsv();
        format!(
            "{}\t{}\n",
            export.strip_suffix('\n').unwrap_or(&export),
            self.app_id
        )
    }

    pub fn export(&self) -> &LauncherExport {
        &self.export
    }

    pub fn name(&self) -> &str {
        self.export.name()
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherTable {
    entries: Vec<LauncherRow>,
}

impl LauncherTable {
    pub fn new(mut entries: Vec<LauncherRow>) -> Result<Self, String> {
        if entries.len() > MAX_APPLICATIONS {
            return Err(format!(
                "launcher table has {} applications; the limit is {MAX_APPLICATIONS}",
                entries.len()
            ));
        }
        entries.sort_by(|left, right| left.name().cmp(right.name()));
        refuse_duplicate_names(entries.iter().map(LauncherRow::name), "launcher table")?;
        // One window app id names one row, or a window would match two.
        let mut app_ids = BTreeSet::new();
        if let Some(row) = entries.iter().find(|row| !app_ids.insert(row.app_id())) {
            return Err(format!(
                "launcher table gives app id {:?} to two applications",
                row.app_id()
            ));
        }
        let table = Self { entries };
        ensure_size(
            "launcher table",
            &table.to_tsv(),
            MAX_APPLICATION_TABLE_BYTES,
        )?;
        Ok(table)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        ensure_text("launcher table", text, MAX_APPLICATION_TABLE_BYTES)?;
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            entries.push(
                LauncherRow::parse(line)
                    .map_err(|error| format!("launcher table row {}: {error}", index + 1))?,
            );
        }
        let table = Self::new(entries)?;
        if table.to_tsv() != text {
            return Err("launcher table is not canonical".into());
        }
        Ok(table)
    }

    pub fn to_tsv(&self) -> String {
        self.entries.iter().map(LauncherRow::to_tsv).collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = &LauncherRow> {
        self.entries.iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRegistry {
    entries: Vec<(String, String)>,
}

impl ApplicationRegistry {
    pub fn new(mut entries: Vec<(String, String)>) -> Result<Self, String> {
        if entries.len() > MAX_APPLICATIONS {
            return Err(format!(
                "application registry has {} applications; the limit is {MAX_APPLICATIONS}",
                entries.len()
            ));
        }
        for (name, path) in &entries {
            validate_application_identity(name)?;
            validate_package_path(path)?;
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        refuse_duplicate_names(
            entries.iter().map(|(name, _)| name.as_str()),
            "application registry",
        )?;
        let registry = Self { entries };
        ensure_size(
            "application registry",
            &registry.to_tsv(),
            MAX_APPLICATION_TABLE_BYTES,
        )?;
        Ok(registry)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        ensure_text("application registry", text, MAX_APPLICATION_TABLE_BYTES)?;
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let fields = split_fields(line, 2, "application registry")
                .map_err(|error| format!("application registry row {}: {error}", index + 1))?;
            let name = fields
                .first()
                .ok_or_else(|| format!("application registry row {} lacks a name", index + 1))?;
            let path = fields
                .get(1)
                .ok_or_else(|| format!("application registry row {} lacks a path", index + 1))?;
            entries.push(((*name).to_string(), (*path).to_string()));
        }
        let registry = Self::new(entries)?;
        if registry.to_tsv() != text {
            return Err("application registry is not canonical".into());
        }
        Ok(registry)
    }

    pub fn to_tsv(&self) -> String {
        self.entries
            .iter()
            .map(|(name, path)| format!("{name}\t{path}\n"))
            .collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries
            .iter()
            .map(|(name, path)| (name.as_str(), path.as_str()))
    }
}

fn validate_display_name(value: &str) -> Result<(), String> {
    validate_text_field(
        "application launcher display name",
        value,
        MAX_DISPLAY_NAME_BYTES,
        true,
    )?;
    if value.trim() != value {
        return Err("application launcher display name has leading or trailing whitespace".into());
    }
    Ok(())
}

fn validate_search_terms<'a>(terms: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let terms: Vec<&str> = terms.collect();
    if terms.len() > MAX_SEARCH_TERMS {
        return Err(format!(
            "application launcher has {} search terms; the limit is {MAX_SEARCH_TERMS}",
            terms.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for term in terms {
        validate_text_field(
            "application launcher search term",
            term,
            MAX_SEARCH_TERM_BYTES,
            true,
        )?;
        if term.chars().any(char::is_whitespace) {
            return Err(format!(
                "application launcher search term {term:?} contains whitespace"
            ));
        }
        if !seen.insert(term) {
            return Err(format!(
                "duplicate application launcher search term {term:?}"
            ));
        }
    }
    Ok(())
}

/// A window app id as the compositor matches one: one to 128 ASCII
/// letters, digits, `.`, `_` and `-`.
fn validate_app_id(app_id: &str) -> Result<(), String> {
    if app_id.is_empty() || app_id.len() > MAX_APP_ID_BYTES {
        return Err(format!(
            "launcher app id is {} bytes; it must be 1 to {MAX_APP_ID_BYTES}",
            app_id.len()
        ));
    }
    if !app_id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "launcher app id {app_id:?} uses a byte outside ASCII letters, digits, `.`, `_` and `-`"
        ));
    }
    Ok(())
}

fn validate_package_path(path: &str) -> Result<(), String> {
    validate_text_field(
        "application package path",
        path,
        MAX_PACKAGE_PATH_BYTES,
        true,
    )?;
    if !path.starts_with('/') {
        return Err("application package path is not absolute".into());
    }
    if path.ends_with('/') {
        return Err("application package path has a trailing slash".into());
    }
    if path
        .split('/')
        .skip(1)
        .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err("application package path is not canonical".into());
    }
    Ok(())
}

fn validate_text_field(
    what: &str,
    value: &str,
    limit: usize,
    require_nonempty: bool,
) -> Result<(), String> {
    if require_nonempty && value.is_empty() {
        return Err(format!("{what} is empty"));
    }
    if value.len() > limit {
        return Err(format!(
            "{what} is {} bytes; the limit is {limit}",
            value.len()
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(format!("{what} contains a control character"));
    }
    Ok(())
}

fn ensure_text(what: &str, text: &str, limit: usize) -> Result<(), String> {
    ensure_size(what, text, limit)?;
    if text.contains('\r') {
        return Err(format!("{what} contains a carriage return"));
    }
    if text.contains('\0') {
        return Err(format!("{what} contains a NUL byte"));
    }
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(format!("{what} lacks a trailing newline"));
    }
    Ok(())
}

fn ensure_size(what: &str, text: &str, limit: usize) -> Result<(), String> {
    if text.len() > limit {
        return Err(format!(
            "{what} is {} bytes; the limit is {limit}",
            text.len()
        ));
    }
    Ok(())
}

fn split_fields<'a>(line: &'a str, count: usize, what: &str) -> Result<Vec<&'a str>, String> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() != count {
        return Err(format!(
            "{what} row has {} fields; expected {count}",
            fields.len()
        ));
    }
    Ok(fields)
}

fn refuse_duplicate_names<'a>(
    names: impl Iterator<Item = &'a str>,
    what: &str,
) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            return Err(format!("{what} contains duplicate application {name:?}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declaration() -> LauncherDeclaration {
        LauncherDeclaration::new("Ripgrep", &["ripgrep", "rg", "search", "text"]).unwrap()
    }

    #[test]
    fn declarations_bind_to_canonical_package_exports() {
        let declaration = declaration();
        assert_eq!(
            LauncherDeclaration::from_json(&declaration.to_json()).unwrap(),
            declaration
        );
        let export = declaration.bind("ripgrep-seed").unwrap();
        let text = "ripgrep-seed\tRipgrep\tripgrep rg search text\n";
        assert_eq!(export.to_tsv(), text);
        assert_eq!(LauncherExport::parse(text).unwrap(), export);
    }

    #[test]
    fn tables_sort_refuse_duplicates_and_round_trip() {
        let ripgrep = declaration().bind("ripgrep-seed").unwrap();
        let editor = LauncherDeclaration::new("Editor", &["edit", "text"])
            .unwrap()
            .bind("editor")
            .unwrap();
        let row = |export: &LauncherExport, app_id: &str| {
            LauncherRow::new(export.clone(), app_id).unwrap()
        };
        let table = LauncherTable::new(vec![
            row(&ripgrep, "org.td.ripgrep"),
            row(&editor, "td-editor"),
        ])
        .unwrap();
        let text = table.to_tsv();
        assert_eq!(
            text,
            "editor\tEditor\tedit text\ttd-editor\n\
             ripgrep-seed\tRipgrep\tripgrep rg search text\torg.td.ripgrep\n"
        );
        assert_eq!(LauncherTable::parse(&text).unwrap(), table);
        let rows: Vec<_> = table
            .entries()
            .map(|row| (row.name(), row.app_id(), row.export().display_name()))
            .collect();
        assert_eq!(
            rows,
            [
                ("editor", "td-editor", "Editor"),
                ("ripgrep-seed", "org.td.ripgrep", "Ripgrep")
            ]
        );
        assert!(LauncherTable::new(vec![row(&ripgrep, "a"), row(&ripgrep, "b")]).is_err());
        let error =
            LauncherTable::new(vec![row(&ripgrep, "same"), row(&editor, "same")]).unwrap_err();
        assert!(error.contains("two applications"), "{error}");
        // A row with no search terms keeps its empty column.
        let bare = LauncherDeclaration::new("Bare", &[])
            .unwrap()
            .bind("bare")
            .unwrap();
        let bare = LauncherTable::new(vec![row(&bare, "bare")]).unwrap();
        assert_eq!(bare.to_tsv(), "bare\tBare\t\tbare\n");
        assert_eq!(LauncherTable::parse("bare\tBare\t\tbare\n").unwrap(), bare);

        let registry = ApplicationRegistry::new(vec![
            (
                "ripgrep-seed".into(),
                "/td/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ripgrep-seed-15.2.0".into(),
            ),
            (
                "editor".into(),
                "/td/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-editor-1".into(),
            ),
        ])
        .unwrap();
        let registry_text = registry.to_tsv();
        assert!(registry_text.starts_with("editor\t"), "{registry_text}");
        assert_eq!(
            ApplicationRegistry::parse(&registry_text).unwrap(),
            registry
        );
    }

    #[test]
    fn malformed_or_ambiguous_rows_are_refused() {
        for text in [
            "ripgrep-seed\tRipgrep\n",
            "ripgrep-seed\tRipgrep\trg\textra\n",
            "ripgrep-seed\t Ripgrep\trg\n",
            "ripgrep-seed\tRipgrep\trg  search\n",
            "ripgrep-seed\tRipgrep\trg\r\n",
        ] {
            assert!(LauncherExport::parse(text).is_err(), "accepted {text:?}");
        }
        // The table's fourth column is the window app id, in its own grammar.
        for text in [
            "ripgrep-seed\tRipgrep\trg\n",
            "ripgrep-seed\tRipgrep\trg\t\n",
            "ripgrep-seed\tRipgrep\trg\torg.td.rg\textra\n",
            "ripgrep-seed\tRipgrep\trg\torg td rg\n",
            "ripgrep-seed\tRipgrep\trg\torg/td/rg\n",
            "ripgrep-seed\tRipgrep\trg\tkéy\n",
            "b\tB\t\tb\na\tA\t\ta\n",
        ] {
            assert!(LauncherTable::parse(text).is_err(), "accepted {text:?}");
        }
        let export = declaration().bind("ripgrep-seed").unwrap();
        assert!(LauncherRow::new(export.clone(), &"x".repeat(MAX_APP_ID_BYTES)).is_ok());
        assert!(LauncherRow::new(export, &"x".repeat(MAX_APP_ID_BYTES + 1)).is_err());
        for text in [
            "app\trelative\n",
            "app\t/td/store/item/\n",
            "app\t/td/store/../item\n",
            "b\t/td/store/b\na\t/td/store/a\n",
        ] {
            assert!(
                ApplicationRegistry::parse(text).is_err(),
                "accepted {text:?}"
            );
        }
    }

    #[test]
    fn launcher_names_cannot_select_td_jail_internal_modes() {
        for name in RESERVED_LAUNCHER_NAMES {
            assert!(LauncherDeclaration::new("Reserved", &[])
                .unwrap()
                .bind(name)
                .is_err());
            assert!(ApplicationRegistry::new(vec![(
                (*name).to_string(),
                "/td/store/package".into()
            ),])
            .is_err());
        }
    }

    #[test]
    fn every_authored_field_and_aggregate_is_bounded() {
        assert!(LauncherDeclaration::new("", &[]).is_err());
        assert!(LauncherDeclaration::new(&"x".repeat(MAX_DISPLAY_NAME_BYTES + 1), &[]).is_err());
        assert!(LauncherDeclaration::new("App", &["two words"]).is_err());
        let many = vec!["x"; MAX_SEARCH_TERMS + 1];
        assert!(LauncherDeclaration::new("App", &many).is_err());
        let entries = (0..=MAX_APPLICATIONS)
            .map(|index| {
                let export = LauncherDeclaration::new("App", &[])
                    .unwrap()
                    .bind(&format!("app-{index}"))
                    .unwrap();
                LauncherRow::new(export, "app").unwrap()
            })
            .collect();
        assert!(LauncherTable::new(entries).is_err());
    }
}
