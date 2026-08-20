//! Canonical application launcher metadata and the two image tables derived from it.

use crate::application::validate_application_name;
use crate::json::Json;
use std::collections::BTreeSet;

pub const MAX_DISPLAY_NAME_BYTES: usize = 128;
pub const MAX_SEARCH_TERM_BYTES: usize = 64;
pub const MAX_SEARCH_TERMS: usize = 32;
pub const MAX_LAUNCHER_EXPORT_BYTES: usize = 4096;
pub const MAX_APPLICATION_TABLE_BYTES: usize = 1024 * 1024;
pub const MAX_APPLICATIONS: usize = 256;
pub const MAX_PACKAGE_PATH_BYTES: usize = 4096;

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
        validate_application_name(name)?;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherTable {
    entries: Vec<LauncherExport>,
}

impl LauncherTable {
    pub fn new(mut entries: Vec<LauncherExport>) -> Result<Self, String> {
        if entries.len() > MAX_APPLICATIONS {
            return Err(format!(
                "launcher table has {} applications; the limit is {MAX_APPLICATIONS}",
                entries.len()
            ));
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        refuse_duplicate_names(entries.iter().map(LauncherExport::name), "launcher table")?;
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
            let row = format!("{line}\n");
            entries.push(
                LauncherExport::parse(&row)
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
        self.entries.iter().map(LauncherExport::to_tsv).collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = &LauncherExport> {
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
            validate_application_name(name)?;
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
        let table = LauncherTable::new(vec![ripgrep.clone(), editor]).unwrap();
        let text = table.to_tsv();
        assert!(text.starts_with("editor\t"), "{text}");
        assert_eq!(LauncherTable::parse(&text).unwrap(), table);
        assert!(LauncherTable::new(vec![ripgrep.clone(), ripgrep]).is_err());

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
    fn every_authored_field_and_aggregate_is_bounded() {
        assert!(LauncherDeclaration::new("", &[]).is_err());
        assert!(LauncherDeclaration::new(&"x".repeat(MAX_DISPLAY_NAME_BYTES + 1), &[]).is_err());
        assert!(LauncherDeclaration::new("App", &["two words"]).is_err());
        let many = vec!["x"; MAX_SEARCH_TERMS + 1];
        assert!(LauncherDeclaration::new("App", &many).is_err());
        let entries = (0..=MAX_APPLICATIONS)
            .map(|index| {
                LauncherDeclaration::new("App", &[])
                    .unwrap()
                    .bind(&format!("app-{index}"))
                    .unwrap()
            })
            .collect();
        assert!(LauncherTable::new(entries).is_err());
    }
}
