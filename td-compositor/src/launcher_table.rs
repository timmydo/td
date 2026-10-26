//! The image's launcher table, `/etc/td-launcher.tsv` (APPLICATIONS.md
//! §A.0): one row per application the image declares, its registry name,
//! display name, search terms and the app id its windows announce. The
//! builder wrote and checked it and the image holds it immutable; this
//! reader holds it to the builder's bounds again and refuses anything
//! else, as the bus application policy is refused.

use std::io::Read;

/// Where the image installs it.
pub(crate) const PATH: &str = "/etc/td-launcher.tsv";
/// The builder's bounds on the table and its rows.
const MAX_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ROWS: usize = 256;
const MAX_DISPLAY_BYTES: usize = 128;
const MAX_TERMS: usize = 32;
const MAX_TERM_BYTES: usize = 64;
const MAX_APP_ID_BYTES: usize = 128;
/// One byte past the bound, so a longer table reads as too long.
const READ: u64 = MAX_BYTES as u64 + 1;

/// One application the card can show.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Row {
    /// The registry name, which every request names.
    pub(crate) name: String,
    pub(crate) display: String,
    pub(crate) terms: Vec<String>,
    /// What `xdg_toplevel.set_app_id` its windows give.
    pub(crate) app_id: String,
}

/// Reads the table, bounded; a missing or malformed table is an error.
pub(crate) fn load() -> Result<Vec<Row>, String> {
    let mut text = String::new();
    std::fs::File::open(PATH)
        .and_then(|file| file.take(READ).read_to_string(&mut text))
        .map_err(|error| format!("read {PATH}: {error}"))?;
    parse(&text).map_err(|error| format!("{PATH}: {error}"))
}

/// The rows of `text`, each `NAME<TAB>DISPLAY<TAB>TERMS<TAB>APP-ID` and a
/// newline, in the builder's bounds, no name or app id twice. A row whose
/// name is outside the registry grammar every request names applications
/// in is kept out: the builder's identity grammar is wider, and no request
/// could start it.
pub(crate) fn parse(text: &str) -> Result<Vec<Row>, String> {
    if text.len() > MAX_BYTES {
        return Err("the launcher table is too large".into());
    }
    if !(text.is_empty() || text.ends_with('\n')) {
        return Err("the launcher table lacks a final newline".into());
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut lines = 0usize;
    for line in text.split_terminator('\n') {
        lines = lines.saturating_add(1);
        if lines > MAX_ROWS {
            return Err("the launcher table has too many rows".into());
        }
        let row = row(line).ok_or_else(|| format!("launcher table row {lines} is malformed"))?;
        if rows
            .iter()
            .any(|known| known.name == row.name || known.app_id == row.app_id)
        {
            return Err(format!(
                "launcher table row {lines} repeats a name or app id"
            ));
        }
        rows.push(row);
    }
    rows.retain(|row| crate::authority::consent::application_name(&row.name));
    Ok(rows)
}

fn row(line: &str) -> Option<Row> {
    let mut fields = line.split('\t');
    let (name, display, terms, app_id) = (
        fields.next()?,
        fields.next()?,
        fields.next()?,
        fields.next()?,
    );
    if fields.next().is_some() {
        return None;
    }
    let text = |value: &str, limit: usize| {
        !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
    };
    let terms: Vec<String> = if terms.is_empty() {
        Vec::new()
    } else {
        terms.split(' ').map(str::to_string).collect()
    };
    let valid = !name.is_empty()
        && name.len() <= 64
        && text(display, MAX_DISPLAY_BYTES)
        && display.trim() == display
        && terms.len() <= MAX_TERMS
        && terms.iter().all(|term| text(term, MAX_TERM_BYTES))
        && !app_id.is_empty()
        && app_id.len() <= MAX_APP_ID_BYTES
        && app_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    valid.then(|| Row {
        name: name.to_string(),
        display: display.to_string(),
        terms,
        app_id: app_id.to_string(),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The image's three applications, as the card and the control tests
    /// read them.
    pub(crate) fn image() -> Vec<Row> {
        parse(IMAGE).unwrap_or_default()
    }

    const IMAGE: &str = "firefox\tFirefox\tbrowser web\torg.mozilla.firefox\n\
                         mail\tMail\temail\ttd-mail\n\
                         news\tNews\t\ttd-news\n";

    #[test]
    fn the_image_table_reads_as_the_builder_writes_it() {
        let rows = parse(IMAGE).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.first(),
            Some(&Row {
                name: "firefox".into(),
                display: "Firefox".into(),
                terms: vec!["browser".into(), "web".into()],
                app_id: "org.mozilla.firefox".into(),
            })
        );
        assert_eq!(rows.get(2).map(|row| row.terms.len()), Some(0));
        assert!(parse("").unwrap().is_empty());
    }

    #[test]
    fn anything_else_is_refused() {
        for bad in [
            "firefox\tFirefox\tbrowser\torg.mozilla.firefox",
            "firefox\tFirefox\tbrowser\n",
            "firefox\tFirefox\tbrowser\torg.mozilla.firefox\textra\n",
            "\tFirefox\tbrowser\tid\n",
            "firefox\t\tbrowser\tid\n",
            "firefox\t Firefox\tbrowser\tid\n",
            "firefox\tFire\u{7}fox\tbrowser\tid\n",
            "firefox\tFirefox\tbrowser  web\tid\n",
            "firefox\tFirefox\tbrowser\t\n",
            "firefox\tFirefox\tbrowser\tid/x\n",
            "firefox\tFirefox\tbrowser\tid\r\n",
            "fire/fox\tFirefox\tbrowser\tid\n",
            "a\tA\t\tid\nb\tB\t\tid\n",
            "a\tA\t\tx\na\tB\t\ty\n",
            "a\tA\t\tx\n\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let long = format!("a\t{}\t\tx\n", "D".repeat(MAX_DISPLAY_BYTES + 1));
        assert!(parse(&long).is_err());
        let id = format!("a\tA\t\t{}\n", "i".repeat(MAX_APP_ID_BYTES + 1));
        assert!(parse(&id).is_err());
        let rows: String = (0..=MAX_ROWS)
            .map(|n| format!("a{n}\tA\t\tid{n}\n"))
            .collect();
        assert!(parse(&rows).is_err());
        let fits: String = (0..MAX_ROWS)
            .map(|n| format!("a{n}\tA\t\tid{n}\n"))
            .collect();
        assert_eq!(parse(&fits).unwrap().len(), MAX_ROWS);
    }

    /// The one file it reads is the image's table, bounded.
    #[test]
    fn it_reads_only_the_image_table_and_only_so_much() {
        let source = include_str!("launcher_table.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or_default();
        assert_eq!(production.matches("File::open(").count(), 1);
        assert!(production.contains("std::fs::File::open(PATH)"));
        assert!(production.contains("file.take(READ).read_to_string(&mut text)"));
        assert!(production.contains("const READ: u64 = MAX_BYTES as u64 + 1;"));
        assert!(production.contains("pub(crate) const PATH: &str = \"/etc/td-launcher.tsv\";"));
        assert!(!production.contains("std::fs::read"));
    }

    /// A builder-legal name no request can name is left off the card.
    #[test]
    fn a_name_outside_the_request_grammar_is_kept_out() {
        let rows = parse("Foo\tFoo\t\tfoo\nmail\tMail\t\ttd-mail\n").unwrap();
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["mail"]
        );
    }
}
