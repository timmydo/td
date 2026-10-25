use super::{body_checks, deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::EmailId, postgres_probe::Request, postgres_read as read, postgres_runtime_profile::Binding,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    password: &[u8],
    emails: [EmailId; 2],
) {
    sql(fixture, "ALTER SYSTEM SET log_statement='all'");
    sql(fixture, "SELECT pg_reload_conf()");
    let until = deadline();
    while sql(fixture, "SHOW log_statement") != "all" {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let offset = std::fs::metadata(fixture.root.join("server.log"))
        .unwrap()
        .len() as usize;
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let mut ids = vec![emails[1], emails[0], emails[1]];
        for n in 0..13 {
            let mut bytes = [0xed; 16];
            bytes[15] = n;
            ids.push(EmailId::from_bytes(bytes));
        }
        assert_eq!(ids.len(), td_mta::postgres_body::MAX_METADATA_IDS);
        view.prefetch_metadata(&ids)?;
        for _ in 0..2 {
            for id in &ids {
                let value = view.email(*id)?;
                assert_eq!(value.is_some(), emails.contains(id));
                if let Some(value) = value {
                    assert_eq!(value.email, *id);
                    assert_eq!(value.identity, view.identity());
                }
            }
        }
        view.prefetch_metadata(&[])?;
        assert!(view.email(emails[0])?.is_some());
        Ok(())
    })
    .unwrap();
    body_checks::quiet(fixture);
    let log = std::fs::read_to_string(fixture.root.join("server.log")).unwrap();
    let queries = log[offset..]
        .lines()
        .filter(|line| {
            line.contains("execute ")
                && line.contains("SELECT a.sequence,a.floor,e.blob_id,e.thread_id")
        })
        .count();
    assert_eq!(
        queries, 2,
        "one batch query plus one lookup after clearing the window"
    );
    sql(fixture, "ALTER SYSTEM RESET log_statement");
    sql(fixture, "SELECT pg_reload_conf()");

    let failure = read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        assert_eq!(
            view.prefetch_metadata(&[emails[0]; 17]),
            Err(read::Error::Capacity)
        );
        assert_eq!(view.email(emails[0]), Err(read::Error::Capacity));
        Ok(())
    });
    assert_eq!(failure, Err(read::Error::Capacity));
}
