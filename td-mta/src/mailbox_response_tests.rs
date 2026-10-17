#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use crate::{
    format::Sequence,
    ids::{AccountId, StoreEpoch},
};
fn identity() -> ViewIdentity {
    ViewIdentity {
        account: AccountId::from_bytes([1; 16]),
        epoch: StoreEpoch::from_bytes([2; 16]),
        committed_sequence: Sequence::from_u64(3),
        history_floor: Sequence::default(),
    }
}
const ID: MailboxId = MailboxId::from_bytes([4; 16]);
fn rights() -> Rights {
    Rights {
        may_read_items: true,
        may_add_items: false,
        may_remove_items: true,
        may_set_seen: false,
        may_set_keywords: true,
        may_create_child: false,
        may_rename: true,
        may_delete: false,
        may_submit: false,
    }
}
fn counts() -> MailboxCounts {
    MailboxCounts {
        identity: identity(),
        mailbox: ID,
        total_emails: 9,
        unread_emails: 4,
        total_threads: 7,
        unread_threads: 3,
    }
}
fn encode(
    projection: Projection,
    counts: Option<&MailboxCounts>,
    out: &mut [u8],
) -> Result<usize, Error> {
    encode_object(
        Object {
            identity: identity(),
            id: ID,
            row: MailboxRow {
                name: "folder \"quoted\"",
                parent: None,
                role: None,
                sort_order: 9,
                subscribed: true,
            },
            counts,
        },
        projection,
        rights(),
        out,
    )
}
#[test]
fn full_projection_preserves_counts_rights_and_escaping() {
    let mut output = [0; 2048];
    let n = encode(Projection::all(), Some(&counts()), &mut output).unwrap();
    let json = td_json::parse(std::str::from_utf8(&output[..n]).unwrap()).unwrap();
    assert_eq!(json.as_obj().unwrap().len(), 11);
    assert_eq!(json["name"].as_str(), Some("folder \"quoted\""));
    for (name, value) in [
        ("totalEmails", 9),
        ("unreadEmails", 4),
        ("totalThreads", 7),
        ("unreadThreads", 3),
    ] {
        assert_eq!(json[name].as_u64(), Some(value));
    }
    for (name, value) in [
        ("mayReadItems", true),
        ("mayAddItems", false),
        ("mayRemoveItems", true),
        ("maySetSeen", false),
        ("maySetKeywords", true),
        ("mayCreateChild", false),
        ("mayRename", true),
        ("mayDelete", false),
        ("maySubmit", false),
    ] {
        assert_eq!(json["myRights"][name].as_bool(), Some(value));
    }
    assert_eq!(
        encode(Projection::all(), Some(&counts()), &mut vec![0; n]).unwrap(),
        n
    );
    assert_eq!(
        encode(Projection::all(), Some(&counts()), &mut vec![0; n - 1]),
        Err(Error::Capacity)
    );
}
#[test]
fn projection_keeps_id_and_rejects_unbound_counts() {
    let mut output = [0; 2048];
    let n = encode(Projection::id_only(), None, &mut output).unwrap();
    let json = td_json::parse(std::str::from_utf8(&output[..n]).unwrap()).unwrap();
    assert_eq!(json.as_obj().unwrap().len(), 1);
    assert_eq!(
        encode(Projection::all(), None, &mut output),
        Err(Error::Invalid)
    );
    let mut wrong = counts();
    wrong.identity.account = AccountId::from_bytes([9; 16]);
    assert_eq!(
        encode(Projection::all(), Some(&wrong), &mut output),
        Err(Error::Invalid)
    );
    let mut wrong = counts();
    wrong.mailbox = MailboxId::from_bytes([9; 16]);
    assert_eq!(
        encode(Projection::all(), Some(&wrong), &mut output),
        Err(Error::Invalid)
    );
    let mut wrong = counts();
    wrong.total_emails = 9_007_199_254_740_992;
    assert_eq!(
        encode(Projection::all(), Some(&wrong), &mut output),
        Err(Error::Invalid)
    );
    assert_eq!(
        Projection::id_only().include("unknown"),
        Err(Error::Invalid)
    );
}
