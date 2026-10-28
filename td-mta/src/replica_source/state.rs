//! Complete mailbox graphs and independently bound change streams.
use super::{fields, local, text, Binding, Error};
use crate::{
    format::{
        key::Key,
        row::{MailboxRow, Row},
    },
    ids::{EmailId, MailboxId},
    sync::{DataState, DataType},
    wire,
};
use td_json::Json;

pub struct Changes<T> {
    pub old: DataState,
    pub new: DataState,
    pub more: bool,
    pub created: Vec<T>,
    pub updated: Vec<T>,
    pub destroyed: Vec<T>,
}
fn bound_state(value: &str, binding: Binding, kind: DataType) -> Result<DataState, Error> {
    let state = DataState::decode(value).map_err(|_| Error::Invalid)?;
    if state.account != binding.account || state.epoch != binding.epoch {
        return Err(Error::SourceChanged);
    }
    if state.kind != kind {
        return Err(Error::Invalid);
    }
    Ok(state)
}
pub fn email_changes(
    value: &Json,
    binding: Binding,
    since: DataState,
    maximum: usize,
) -> Result<Changes<EmailId>, Error> {
    change_page(value, binding, since, maximum, DataType::Email)
}
pub fn mailbox_changes(
    value: &Json,
    binding: Binding,
    since: DataState,
    maximum: usize,
) -> Result<Changes<MailboxId>, Error> {
    change_page(value, binding, since, maximum, DataType::Mailbox)
}
fn change_page<T: wire::LocalId + Ord>(
    value: &Json,
    binding: Binding,
    since: DataState,
    maximum: usize,
    kind: DataType,
) -> Result<Changes<T>, Error> {
    if maximum == 0 || maximum > crate::jmap_changes::MAX_CHANGES {
        return Err(Error::Capacity);
    }
    if since.account != binding.account || since.epoch != binding.epoch {
        return Err(Error::SourceChanged);
    }
    if since.kind != kind {
        return Err(Error::Invalid);
    }
    if kind == DataType::Mailbox {
        fields(
            value,
            &[
                "accountId",
                "oldState",
                "newState",
                "hasMoreChanges",
                "created",
                "updated",
                "destroyed",
                "updatedProperties",
            ],
        )?;
        if value.get("updatedProperties") != Some(&Json::Null) {
            return Err(Error::Unsupported);
        }
    } else {
        fields(
            value,
            &[
                "accountId",
                "oldState",
                "newState",
                "hasMoreChanges",
                "created",
                "updated",
                "destroyed",
            ],
        )?;
    }
    if local::<crate::ids::AccountId>(text(value, "accountId")?)? != binding.account {
        return Err(Error::SourceChanged);
    }
    let old = bound_state(text(value, "oldState")?, binding, kind)?;
    let new = bound_state(text(value, "newState")?, binding, kind)?;
    let more = value
        .get("hasMoreChanges")
        .and_then(Json::as_bool)
        .ok_or(Error::Invalid)?;
    if old != since || new.sequence < old.sequence || (more && new.sequence == old.sequence) {
        return Err(Error::Invalid);
    }
    let mut remaining = maximum;
    let mut ids = |key| -> Result<Vec<T>, Error> {
        let values = value
            .get(key)
            .and_then(Json::as_array)
            .ok_or(Error::Invalid)?;
        remaining = remaining.checked_sub(values.len()).ok_or(Error::Capacity)?;
        let mut ids = Vec::new();
        ids.try_reserve_exact(values.len())
            .map_err(|_| Error::Capacity)?;
        for value in values {
            ids.push(local::<T>(value.as_str().ok_or(Error::Invalid)?)?);
        }
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(Error::Invalid);
        }
        Ok(ids)
    };
    let created = ids("created")?;
    let updated = ids("updated")?;
    let destroyed = ids("destroyed")?;
    if created
        .iter()
        .any(|id| updated.binary_search(id).is_ok() || destroyed.binary_search(id).is_ok())
        || updated.iter().any(|id| destroyed.binary_search(id).is_ok())
        || (remaining != maximum && new.sequence == old.sequence)
    {
        return Err(Error::Invalid);
    }
    Ok(Changes {
        old,
        new,
        more,
        created,
        updated,
        destroyed,
    })
}

pub struct Mailbox<'a> {
    pub id: MailboxId,
    pub row: MailboxRow<'a>,
}
pub struct Mailboxes<'a> {
    pub observed: DataState,
    pub list: Vec<Mailbox<'a>>,
}

/// Decode a complete Mailbox/get with ids:null and the six stored properties.
/// A dependency missing from a later Email response requires fresh reconciliation.
pub fn mailboxes(value: &Json, binding: Binding) -> Result<Mailboxes<'_>, Error> {
    fields(value, &["accountId", "state", "list", "notFound"])?;
    if local::<crate::ids::AccountId>(text(value, "accountId")?)? != binding.account {
        return Err(Error::SourceChanged);
    }
    let observed = bound_state(text(value, "state")?, binding, DataType::Mailbox)?;
    if !value
        .get("notFound")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?
        .is_empty()
    {
        return Err(Error::Invalid);
    }
    let values = value
        .get("list")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    if values.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
        return Err(Error::Capacity);
    }
    let mut list = Vec::new();
    list.try_reserve_exact(values.len())
        .map_err(|_| Error::Capacity)?;
    for value in values {
        fields(
            value,
            &[
                "id",
                "name",
                "parentId",
                "role",
                "sortOrder",
                "isSubscribed",
            ],
        )?;
        let id = local(text(value, "id")?)?;
        let parent = match value.get("parentId").ok_or(Error::Invalid)? {
            Json::Null => None,
            value => Some(local(value.as_str().ok_or(Error::Invalid)?)?),
        };
        let role = match value.get("role").ok_or(Error::Invalid)? {
            Json::Null => None,
            value => Some(value.as_str().ok_or(Error::Invalid)?),
        };
        let row = MailboxRow {
            name: text(value, "name")?,
            parent,
            role,
            sort_order: value
                .get("sortOrder")
                .and_then(Json::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(Error::Invalid)?,
            subscribed: value
                .get("isSubscribed")
                .and_then(Json::as_bool)
                .ok_or(Error::Invalid)?,
        };
        Row::Mailbox(row)
            .validate_key(Key::Mailbox(id))
            .map_err(|_| Error::Invalid)?;
        list.push(Mailbox { id, row });
    }
    list.sort_unstable_by_key(|entry| entry.id);
    if list
        .windows(2)
        .any(|pair| pair.first().map(|entry| entry.id) == pair.get(1).map(|entry| entry.id))
    {
        return Err(Error::Invalid);
    }
    // Finite parent walks also reject missing parents and every cycle.
    for (index, mailbox) in list.iter().enumerate() {
        if mailbox.row.role.is_some()
            && list
                .get(..index)
                .ok_or(Error::Invalid)?
                .iter()
                .any(|entry| entry.row.role == mailbox.row.role)
        {
            return Err(Error::Invalid);
        }
        let mut parent = mailbox.row.parent;
        let mut remaining = list.len();
        while let Some(id) = parent {
            remaining = remaining.checked_sub(1).ok_or(Error::Invalid)?;
            let at = list
                .binary_search_by_key(&id, |entry| entry.id)
                .map_err(|_| Error::Invalid)?;
            parent = list.get(at).ok_or(Error::Invalid)?.row.parent;
        }
    }
    Ok(Mailboxes { observed, list })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use crate::{
        format::Sequence,
        ids::{AccountId, StoreEpoch},
    };
    use td_json::json;
    fn binding() -> Binding {
        Binding {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
        }
    }
    fn state(kind: DataType, sequence: u64) -> DataState {
        DataState {
            account: binding().account,
            epoch: binding().epoch,
            kind,
            sequence: Sequence::from_u64(sequence),
        }
    }
    fn token(state: DataState) -> String {
        state
            .encode(&mut [0; DataState::WIRE_BYTES])
            .unwrap()
            .to_owned()
    }
    fn changes(kind: DataType) -> Json {
        let mut value = json!({"accountId":"a01010101010101010101010101010101","oldState":token(state(kind,1)),"newState":token(state(kind,3)),"hasMoreChanges":false,"created":[],"updated":[],"destroyed":[]});
        if kind == DataType::Mailbox {
            let _ = value.insert("updatedProperties", Json::Null);
        }
        value
    }
    #[test]
    fn change_streams_bind_kind_scope_and_monotonic_progress() {
        let base = state(DataType::Email, 1);
        let original = changes(DataType::Email);
        let page = email_changes(&original, binding(), base, 3).unwrap();
        assert_eq!(page.new.sequence.number(), 3);
        assert!(page.created.is_empty() && !page.more);
        for (field, replacement) in [
            ("accountId", json!("a03030303030303030303030303030303")),
            ("oldState", json!(token(state(DataType::Email, 0)))),
            ("newState", json!(token(state(DataType::Email, 0)))),
            ("newState", json!(token(state(DataType::Mailbox, 3)))),
            ("hasMoreChanges", json!(null)),
        ] {
            let mut value = original.clone();
            *value.get_mut(field).unwrap() = replacement;
            assert!(email_changes(&value, binding(), base, 3).is_err());
        }
        let mut unchanged = original.clone();
        *unchanged.get_mut("newState").unwrap() = json!(token(base));
        assert!(email_changes(&unchanged, binding(), base, 3).is_ok());
        *unchanged.get_mut("hasMoreChanges").unwrap() = json!(true);
        assert!(email_changes(&unchanged, binding(), base, 3).is_err());
        assert!(mailbox_changes(&original, binding(), base, 3).is_err());
        let mailbox = changes(DataType::Mailbox);
        assert!(mailbox_changes(&mailbox, binding(), state(DataType::Mailbox, 1), 3).is_ok());
        assert!(email_changes(&original, binding(), base, 0).is_err());
        assert!(email_changes(
            &original,
            binding(),
            base,
            crate::jmap_changes::MAX_CHANGES + 1
        )
        .is_err());
    }
    #[test]
    fn change_ids_are_complete_disjoint_and_bounded() {
        let base = state(DataType::Email, 1);
        let mut value = changes(DataType::Email);
        *value.get_mut("created").unwrap() = json!([
            "e04040404040404040404040404040404",
            "e03030303030303030303030303030303"
        ]);
        *value.get_mut("destroyed").unwrap() = json!(["e05050505050505050505050505050505"]);
        let page = email_changes(&value, binding(), base, 3).unwrap();
        assert_eq!(page.created.first(), Some(&EmailId::from_bytes([3; 16])));
        assert_eq!(
            email_changes(&value, binding(), base, 2).err(),
            Some(Error::Capacity)
        );
        *value.get_mut("updated").unwrap() = json!(["e03030303030303030303030303030303"]);
        assert_eq!(
            email_changes(&value, binding(), base, 4).err(),
            Some(Error::Invalid)
        );
        *value.get_mut("updated").unwrap() = json!([]);
        *value.get_mut("destroyed").unwrap() = json!([
            "e05050505050505050505050505050505",
            "e05050505050505050505050505050505"
        ]);
        assert_eq!(
            email_changes(&value, binding(), base, 4).err(),
            Some(Error::Invalid)
        );
        *value.get_mut("destroyed").unwrap() = json!(["m05050505050505050505050505050505"]);
        assert_eq!(
            email_changes(&value, binding(), base, 3).err(),
            Some(Error::Invalid)
        );
    }
    fn list_mut(value: &mut Json) -> &mut Vec<Json> {
        match value.get_mut("list").unwrap() {
            Json::Arr(values) => values,
            _ => panic!("fixture"),
        }
    }
    fn graph() -> Json {
        json!({"accountId":"a01010101010101010101010101010101","state":token(state(DataType::Mailbox,5)),"notFound":[],"list":[
            {"id":"m03030303030303030303030303030303","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},
            {"id":"m04040404040404040404040404040404","name":"child","parentId":"m03030303030303030303030303030303","role":null,"sortOrder":2,"isSubscribed":false}
        ]})
    }
    #[test]
    fn complete_mailbox_graph_preserves_properties_and_refuses_broken_dependencies() {
        let original = graph();
        let page = mailboxes(&original, binding()).unwrap();
        let child = page.list.get(1).unwrap();
        assert_eq!(child.row.parent, Some(MailboxId::from_bytes([3; 16])));
        assert_eq!(child.row.name, "child");
        assert_eq!(child.row.sort_order, 2);
        assert!(!child.row.subscribed);
        for (field, replacement) in [
            ("parentId", json!("m05050505050505050505050505050505")),
            ("parentId", json!("m04040404040404040404040404040404")),
            ("id", json!("m03030303030303030303030303030303")),
            ("role", json!("inbox")),
            ("role", json!("bad role")),
            ("name", json!("")),
            ("sortOrder", json!(2147483648u64)),
            ("sortOrder", json!(-1)),
            ("isSubscribed", json!(1)),
        ] {
            let mut value = original.clone();
            *list_mut(&mut value)
                .get_mut(1)
                .unwrap()
                .get_mut(field)
                .unwrap() = replacement;
            assert!(mailboxes(&value, binding()).is_err(), "{field}");
        }
        let mut cycle = original.clone();
        *list_mut(&mut cycle)
            .first_mut()
            .unwrap()
            .get_mut("parentId")
            .unwrap() = json!("m04040404040404040404040404040404");
        assert!(mailboxes(&cycle, binding()).is_err());
        let mut missing = original.clone();
        *missing.get_mut("notFound").unwrap() = json!(["m05050505050505050505050505050505"]);
        assert!(mailboxes(&missing, binding()).is_err());
        let mut wrong = original.clone();
        *wrong.get_mut("state").unwrap() = json!(token(state(DataType::Email, 5)));
        assert!(mailboxes(&wrong, binding()).is_err());
        let mut excessive = original;
        let item = excessive
            .get("list")
            .unwrap()
            .as_array()
            .unwrap()
            .first()
            .unwrap()
            .clone();
        *excessive.get_mut("list").unwrap() =
            Json::Arr(vec![item; crate::postgres_mailboxes::MAX_MAILBOXES + 1]);
        assert_eq!(
            mailboxes(&excessive, binding()).err(),
            Some(Error::Capacity)
        );
    }
}
