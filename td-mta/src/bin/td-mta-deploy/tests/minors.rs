#![allow(clippy::unwrap_used)]
use super::*;

#[test]
fn the_table_lists_every_release_once_with_named_notes() {
    covered(12).unwrap();
    let mut ids = Vec::new();
    for (i, minor) in MINORS.iter().enumerate() {
        assert_eq!(minor.minor as usize, i + 1);
        for note in minor.notes {
            assert!(
                note.id.starts_with(&format!("17.{}-", minor.minor)),
                "{}",
                note.id
            );
            assert!(!ids.contains(&note.id), "{}", note.id);
            ids.push(note.id);
            match note.check {
                Check::Database(sql) | Check::Cluster(sql) => {
                    assert!(sql.starts_with("SELECT count(*) FROM "), "{}", note.id);
                    assert!(!sql.contains(';'), "{}", note.id);
                }
                Check::Exempt(reason) => assert!(!reason.is_empty()),
                Check::ReindexGin => (),
            }
        }
    }
}

#[test]
fn a_switch_takes_the_notes_after_the_live_release_up_to_tds() {
    let ids = |live, td| {
        span(live, td)
            .unwrap()
            .map(|(_, n)| n.id)
            .collect::<Vec<_>>()
    };
    assert!(ids(12, 12).is_empty());
    assert_eq!(
        ids(10, 12).first().copied(),
        Some("17.11-output-plugin-libraries")
    );
    assert!(ids(10, 12)
        .iter()
        .all(|id| id.starts_with("17.11-") || id.starts_with("17.12-")));
    assert!(ids(0, 12).contains(&"17.1-foreign-key-partition-detach"));
    assert!(!ids(1, 12).contains(&"17.1-foreign-key-partition-detach"));
    assert!(ids(5, 6).contains(&"17.6-brin-numeric-minmax-multi"));
    assert!(!ids(5, 6).contains(&"17.12-gin-vacuum-and-page-deletion"));
    // Backwards, and a release the table does not list, refuse.
    assert!(span(12, 6).is_err());
    let unlisted = span(6, 13).err().unwrap().to_string();
    assert!(unlisted.contains("postgres_minors.rs"), "{unlisted}");
    assert!(covered(13).is_err());
}

// A PostgreSQL pin the table does not cover fails here, not in production.
#[test]
fn the_table_covers_the_pinned_postgresql() {
    let pins = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("recipes/src/source_pins.rs");
    let pins = std::fs::read_to_string(pins).unwrap();
    let marker = "ftp.postgresql.org/pub/source/v17.";
    assert_eq!(pins.matches(marker).count(), 1);
    let (_, rest) = pins.split_once(marker).unwrap();
    let minor: u32 = rest.split_once('/').unwrap().0.parse().unwrap();
    covered(minor).unwrap();
}
