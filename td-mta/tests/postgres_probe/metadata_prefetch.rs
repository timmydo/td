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
        let boxes = view.mailboxes(emails[0], None, 256)?.values;
        let keywords = view.keywords(emails[0], None, 256)?.values;
        view.prefetch_metadata(&ids)?;
        view.prefetch_values(&ids, true, true)?;
        for _ in 0..2 {
            assert_eq!(view.mailboxes(emails[0], None, 256)?.values, boxes);
            assert_eq!(view.keywords(emails[0], None, 256)?.values, keywords);
            if let Some(first) = keywords.first() {
                let page = view.keywords(emails[0], None, 1)?;
                assert_eq!(page.values, vec![first.clone()]);
                assert_eq!(page.has_more, keywords.len() > 1);
                assert_eq!(
                    view.keywords(emails[0], Some(first), 256)?.values,
                    keywords[1..]
                );
            }
            assert!(view
                .mailboxes(*ids.last().unwrap(), None, 256)?
                .values
                .is_empty());
            assert!(view
                .keywords(*ids.last().unwrap(), None, 256)?
                .values
                .is_empty());
        }
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
    let individual = log[offset..].lines().filter(|line| line.contains("execute ") &&
        (line.contains("SELECT m.mailbox_id,m.changed,b.changed FROM") ||
         line.contains("SELECT keyword,changed FROM td_mta.keywords WHERE account=$1 AND email_id=$2"))).count();
    assert_eq!(
        individual, 2,
        "only the uncached reference queries may read values individually"
    );
    let batches = log[offset..]
        .lines()
        .filter(|line| line.contains("execute ") && line.contains("FROM unnest($2::bytea[]) i(id)"))
        .count();
    assert_eq!(batches, 2, "one bounded batch per selected set");
    sql(fixture, "ALTER SYSTEM RESET log_statement");
    sql(fixture, "SELECT pg_reload_conf()");

    sql(fixture, &format!("INSERT INTO td_mta.keywords(account,email_id,keyword,changed) SELECT account,id,'prefetch-' || n,changed FROM td_mta.emails CROSS JOIN generate_series(1,9) n WHERE id=decode('{}','hex')", emails[0]));
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        let expected = view.keywords(emails[0], None, 256)?.values;
        assert!(expected.len() > 8);
        view.prefetch_values(&emails, true, true)?;
        let mut found = Vec::new();
        let mut after = None;
        loop {
            let page = view.keywords(emails[0], after.as_deref(), 256)?;
            let more = page.has_more;
            after = page.values.last().cloned();
            found.extend(page.values);
            if !more {
                break;
            }
            assert!(after.is_some());
        }
        assert_eq!(found, expected);
        let page = view.keywords(emails[0], Some("prefetch-1"), 2)?;
        assert_eq!(page.values, vec!["prefetch-2", "prefetch-3"]);
        assert!(page.has_more);
        view.prefetch_values(&emails, false, false)?;
        assert_eq!(view.keywords(emails[0], None, 256)?.values, expected);
        view.prefetch_metadata(&[])?;
        assert_eq!(view.keywords(emails[0], None, 256)?.values, expected);
        Ok(())
    })
    .unwrap();
    sql(fixture, &format!("DELETE FROM td_mta.keywords WHERE email_id=decode('{}','hex') AND keyword LIKE 'prefetch-%'", emails[0]));

    maximum_prefix_request(fixture, request, trust, binding, password, emails[0]);

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

fn maximum_prefix_request(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
    password: &[u8],
    source: EmailId,
) {
    sql(fixture, &format!("INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT account,decode(repeat('db',14)||lpad(to_hex(n),4,'0'),'hex'),blob_id,thread_id,received_at,2,changed FROM td_mta.emails CROSS JOIN generate_series(0,255) n WHERE id=decode('{source}','hex'); INSERT INTO td_mta.mailboxes(account,id,name,sort_order,subscribed,changed) SELECT account,decode(repeat('dc',15)||lpad(to_hex(n),2,'0'),'hex'),'prefetch-'||n,0,1,changed FROM td_mta.emails CROSS JOIN generate_series(1,9) n WHERE id=decode('{source}','hex'); INSERT INTO td_mta.memberships(account,email_id,mailbox_id,changed) SELECT e.account,e.id,b.id,e.changed FROM td_mta.emails e JOIN td_mta.mailboxes b ON e.account=b.account WHERE substring(e.id from 1 for 14)=decode(repeat('db',14),'hex') AND substring(b.id from 1 for 15)=decode(repeat('dc',15),'hex'); INSERT INTO td_mta.keywords(account,email_id,keyword,changed) SELECT account,id,'prefetch-'||n,changed FROM td_mta.emails CROSS JOIN generate_series(1,9) n WHERE substring(id from 1 for 14)=decode(repeat('db',14),'hex')"));
    let ids: Vec<_> = (0..256u16)
        .map(|n| {
            let mut bytes = [0xdb; 16];
            bytes[14..].copy_from_slice(&n.to_be_bytes());
            EmailId::from_bytes(bytes)
        })
        .collect();
    read::with_snapshot(request, trust, deadline(), binding, password, |view| {
        for ids in ids.chunks(td_mta::postgres_body::MAX_METADATA_IDS) {
            view.prefetch_metadata(ids)?;
            view.prefetch_values(ids, true, true)?;
            for id in ids {
                assert!(view.email(*id)?.is_some());
                let mut after = None;
                let mut count = 0;
                loop {
                    let page = view.mailboxes(*id, after, 256)?;
                    count += page.values.len();
                    after = page.values.last().copied();
                    if !page.has_more {
                        break;
                    }
                    assert!(after.is_some());
                }
                assert_eq!(count, 9);
                let mut after = None;
                let mut count = 0;
                loop {
                    let page = view.keywords(*id, after.as_deref(), 256)?;
                    count += page.values.len();
                    after = page.values.last().cloned();
                    if !page.has_more {
                        break;
                    }
                    assert!(after.is_some());
                }
                assert_eq!(count, 9);
            }
        }
        Ok(())
    })
    .unwrap();
    sql(fixture, "DELETE FROM td_mta.keywords WHERE substring(email_id from 1 for 14)=decode(repeat('db',14),'hex'); DELETE FROM td_mta.memberships WHERE substring(email_id from 1 for 14)=decode(repeat('db',14),'hex'); DELETE FROM td_mta.emails WHERE substring(id from 1 for 14)=decode(repeat('db',14),'hex'); DELETE FROM td_mta.mailboxes WHERE substring(id from 1 for 15)=decode(repeat('dc',15),'hex')");
}
