use super::*;

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &td_mta::postgres_runtime_profile::Binding,
    password: &[u8],
) {
    use td_mta::{ids::EmailId, postgres_read as read};
    let inventory = |cursor, maximum| {
        read::with_snapshot(request, trust, deadline(), binding, password, |view| {
            view.replica_inventory(cursor, maximum)
        })
    };
    let original = inventory(None, 256).unwrap();
    assert_eq!(original.ids.len(), 3);
    assert!(original.next.is_none());
    let first = inventory(None, 1).unwrap();
    assert_eq!(first.ids, original.ids[..1]);
    let cursor = first.next.unwrap();
    assert_eq!(cursor.base, first.base);
    assert_eq!(cursor.upper, *original.ids.last().unwrap());
    assert_eq!(cursor.after, original.ids[0]);
    let account = binding.account();
    let seen = original.ids[0];
    let retained = original.ids[1];
    // Real separate transactions between pages, with deletion before the
    // cursor, moved date ordering, and arrivals on both sides of its bounds.
    sql(fixture, &format!(
        "BEGIN; DELETE FROM td_mta.memberships WHERE account=decode('{account}','hex') AND email_id=decode('{seen}','hex'); DELETE FROM td_mta.keywords WHERE account=decode('{account}','hex') AND email_id=decode('{seen}','hex'); DELETE FROM td_mta.thread_anchors WHERE account=decode('{account}','hex') AND email_id=decode('{seen}','hex'); DELETE FROM td_mta.emails WHERE account=decode('{account}','hex') AND id=decode('{seen}','hex'); UPDATE td_mta.accounts SET sequence=decode('0000000000000006','hex') WHERE id=decode('{account}','hex'); UPDATE td_mta.emails SET received_at=99-received_at,changed=decode('0000000000000006','hex') WHERE account=decode('{account}','hex'); COMMIT;"
    ));
    let shifted = sql(fixture, &format!("SELECT encode(id,'hex') FROM td_mta.emails WHERE account=decode('{account}','hex') ORDER BY id OFFSET 1"));
    assert_eq!(shifted, original.ids[2].to_string());
    assert_eq!(inventory(Some(cursor), 256).unwrap().ids, original.ids[1..]);
    for id in [EmailId::from_bytes([0; 16]), EmailId::from_bytes([255; 16])] {
        sql(fixture, &format!(
            "INSERT INTO td_mta.emails SELECT account,decode('{id}','hex'),blob_id,thread_id,received_at,origin,peer_family,peer_octets,gateway,tls,ehlo,reverse_path,receipt_count,changed FROM td_mta.emails WHERE account=decode('{account}','hex') AND id=decode('{retained}','hex');"
        ));
    }
    let rest = inventory(Some(cursor), 256).unwrap();
    assert_eq!(rest.ids, original.ids[1..]);
    assert!(rest.next.is_none());
    assert_eq!(rest.base, first.base);
    assert!(rest.observed.sequence > rest.base.sequence);
    let fresh = inventory(None, 256).unwrap();
    assert_eq!(fresh.ids.len(), 4);
    assert_eq!(fresh.ids.first(), Some(&EmailId::from_bytes([0; 16])));
    assert_eq!(fresh.ids.last(), Some(&EmailId::from_bytes([255; 16])));
    sql(fixture, &format!("UPDATE td_mta.emails SET changed=decode('0000000000000007','hex') WHERE account=decode('{account}','hex') AND id=decode('{retained}','hex')"));
    assert_eq!(inventory(None, 1).unwrap_err(), read::Error::Integrity);
    sql(fixture, &format!("UPDATE td_mta.emails SET changed=decode('0000000000000006','hex') WHERE account=decode('{account}','hex') AND id=decode('{retained}','hex')"));
    assert!(inventory(
        Some(read::ReplicaInventoryCursor {
            base: td_mta::sync::DataState {
                account: td_mta::ids::AccountId::from_bytes([0xee; 16]),
                ..cursor.base
            },
            ..cursor
        }),
        1
    )
    .is_err());
    assert_eq!(inventory(None, 0).unwrap_err(), read::Error::Invalid);
    assert_eq!(inventory(None, 257).unwrap_err(), read::Error::Capacity);
    sql(
        fixture,
        &format!("UPDATE td_mta.accounts SET floor=sequence WHERE id=decode('{account}','hex')"),
    );
    assert_eq!(
        inventory(Some(cursor), 1).unwrap_err(),
        read::Error::Changes(td_mta::jmap_changes::Error::CannotCalculateChanges)
    );
    assert_eq!(inventory(None, 256).unwrap().ids, fresh.ids);
    sql(fixture, &format!("BEGIN; DELETE FROM td_mta.memberships WHERE account=decode('{account}','hex'); DELETE FROM td_mta.keywords WHERE account=decode('{account}','hex'); DELETE FROM td_mta.thread_anchors WHERE account=decode('{account}','hex'); DELETE FROM td_mta.emails WHERE account=decode('{account}','hex'); COMMIT;"));
    let empty = inventory(None, 256).unwrap();
    assert!(empty.ids.is_empty());
    assert!(empty.next.is_none());
    assert_eq!(empty.base, empty.observed);
}
