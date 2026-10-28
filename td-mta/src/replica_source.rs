//! Passive replica source responses; validated data grants no local publication authority.
use crate::{
    ids::{AccountId, EmailId, StoreEpoch},
    postgres_read::{ReplicaInventoryCursor, MAX_REPLICA_INVENTORY_PAGE},
    sync::{DataState, DataType},
    wire,
};
use td_json::Json;
mod discovery;
pub use discovery::{discover, Discovery, SourceLimits};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Capacity,
    SourceChanged,
    Reset,
    RemoteFailure,
    Unsupported,
}
#[derive(Clone, Copy)]
pub struct Binding {
    pub account: AccountId,
    pub epoch: StoreEpoch,
}
pub struct Inventory {
    pub base: DataState,
    pub observed: DataState,
    pub ids: Vec<EmailId>,
    pub next: Option<ReplicaInventoryCursor>,
}

fn fields(value: &Json, names: &[&str]) -> Result<(), Error> {
    let fields = value.as_object().ok_or(Error::Invalid)?;
    if fields.len() != names.len() || names.len() > 32 {
        return Err(Error::Invalid);
    }
    let mut seen = 0u32;
    for (name, _) in fields {
        let index = names
            .iter()
            .position(|expected| name == expected)
            .ok_or(Error::Invalid)?;
        let bit = 1u32.checked_shl(index as u32).ok_or(Error::Invalid)?;
        if seen & bit != 0 {
            return Err(Error::Invalid);
        }
        seen |= bit;
    }
    Ok(())
}
fn text<'a>(value: &'a Json, key: &str) -> Result<&'a str, Error> {
    value.get(key).and_then(Json::as_str).ok_or(Error::Invalid)
}
fn local<T: wire::LocalId>(value: &str) -> Result<T, Error> {
    wire::Id::parse(value)
        .map_err(|_| Error::Invalid)?
        .local()
        .ok_or(Error::Invalid)
}
fn state(value: &str, binding: Binding) -> Result<DataState, Error> {
    let state = DataState::decode(value).map_err(|_| Error::Invalid)?;
    if state.account != binding.account || state.epoch != binding.epoch {
        return Err(Error::SourceChanged);
    }
    if state.kind != DataType::Email {
        return Err(Error::Invalid);
    }
    Ok(state)
}
fn scope(value: &Json, binding: Binding) -> Result<DataState, Error> {
    let account = local::<AccountId>(text(value, "accountId")?)?;
    let epoch = StoreEpoch::parse(text(value, "sourceId")?).map_err(|_| Error::Invalid)?;
    if account != binding.account || epoch != binding.epoch {
        return Err(Error::SourceChanged);
    }
    state(text(value, "state")?, binding)
}

pub fn inventory(
    value: &Json,
    binding: Binding,
    previous: Option<ReplicaInventoryCursor>,
    maximum: usize,
) -> Result<Inventory, Error> {
    if maximum == 0 || maximum > MAX_REPLICA_INVENTORY_PAGE {
        return Err(Error::Invalid);
    }
    fields(
        value,
        &[
            "accountId",
            "sourceId",
            "baseState",
            "state",
            "ids",
            "nextCursor",
        ],
    )?;
    let observed = scope(value, binding)?;
    let base = state(text(value, "baseState")?, binding)?;
    if base.sequence > observed.sequence {
        return Err(Error::Invalid);
    }
    if previous.is_none() && base != observed {
        return Err(Error::Invalid);
    }
    if let Some(previous) = previous {
        if previous.base != base || previous.after.as_bytes() > previous.upper.as_bytes() {
            return Err(Error::Invalid);
        }
    }
    let source_ids = value
        .get("ids")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    if source_ids.len() > maximum {
        return Err(Error::Capacity);
    }
    let mut ids = Vec::new();
    ids.try_reserve_exact(source_ids.len())
        .map_err(|_| Error::Capacity)?;
    let mut last = previous.map(|cursor| cursor.after);
    for value in source_ids {
        let id = local::<EmailId>(value.as_str().ok_or(Error::Invalid)?)?;
        if last.is_some_and(|last| last.as_bytes() >= id.as_bytes())
            || previous.is_some_and(|cursor| id.as_bytes() > cursor.upper.as_bytes())
        {
            return Err(Error::Invalid);
        }
        ids.push(id);
        last = Some(id);
    }
    let next = match value.get("nextCursor").ok_or(Error::Invalid)? {
        Json::Null => None,
        value => {
            let cursor = ReplicaInventoryCursor::decode(value.as_str().ok_or(Error::Invalid)?)
                .map_err(|_| Error::Invalid)?;
            if cursor.base != base
                || ids.last() != Some(&cursor.after)
                || cursor.after.as_bytes() >= cursor.upper.as_bytes()
                || previous.is_some_and(|old| old.upper != cursor.upper)
                || ids.len() != maximum
            {
                return Err(Error::Invalid);
            }
            Some(cursor)
        }
    };
    Ok(Inventory {
        base,
        observed,
        ids,
        next,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::format::Sequence;
    use td_json::json;
    fn binding() -> Binding {
        Binding {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
        }
    }
    fn page() -> Json {
        let binding = binding();
        let state = DataState {
            account: binding.account,
            epoch: binding.epoch,
            kind: DataType::Email,
            sequence: Sequence::from_u64(9),
        };
        let mut bytes = [0; DataState::WIRE_BYTES];
        let state = state.encode(&mut bytes).unwrap();
        json!({"accountId":"a01010101010101010101010101010101","sourceId":"02020202020202020202020202020202","baseState":state,"state":state,"ids":["e03030303030303030303030303030303"],"nextCursor":null})
    }
    #[test]
    fn complete_page_requires_bound_scope_and_exact_fields() {
        let value = page();
        let result = inventory(&value, binding(), None, 1).unwrap();
        assert_eq!(result.ids, vec![EmailId::from_bytes([3; 16])]);
        assert!(result.next.is_none());
        let mut wrong = binding();
        wrong.epoch = StoreEpoch::from_bytes([9; 16]);
        assert!(matches!(
            inventory(&value, wrong, None, 1),
            Err(Error::SourceChanged)
        ));
        assert!(matches!(
            inventory(&value, binding(), None, 0),
            Err(Error::Invalid)
        ));
        assert!(matches!(
            inventory(&value, binding(), None, 257),
            Err(Error::Invalid)
        ));
        for value in [Json::Null, json!({}), json!({"accountId":"a"})] {
            assert!(inventory(&value, binding(), None, 1).is_err());
        }
    }
    #[test]
    fn inventory_refuses_duplicate_fields_ids_and_out_of_order_windows() {
        let mut value = page();
        *value.get_mut("ids").unwrap() = json!([
            "e03030303030303030303030303030303",
            "e03030303030303030303030303030303"
        ]);
        assert!(inventory(&value, binding(), None, 2).is_err());
        *value.get_mut("ids").unwrap() = json!([
            "e04040404040404040404040404040404",
            "e03030303030303030303030303030303"
        ]);
        assert!(inventory(&value, binding(), None, 2).is_err());
        assert!(matches!(
            inventory(&value, binding(), None, 1),
            Err(Error::Capacity)
        ));
        let encoded = page().to_string();
        let duplicate = encoded.replacen("{", "{\"ids\":[],", 1);
        assert!(td_json::parse(&duplicate).is_err());
    }
    #[test]
    fn continuation_requires_exact_cursor_progress() {
        let original = page();
        let first = inventory(&original, binding(), None, 1).unwrap();
        let cursor = ReplicaInventoryCursor {
            base: first.base,
            upper: EmailId::from_bytes([5; 16]),
            after: first.ids.first().copied().unwrap(),
        };
        let mut bytes = [0; ReplicaInventoryCursor::WIRE_BYTES];
        let encoded = cursor.encode(&mut bytes).unwrap();
        let mut first = original.clone();
        *first.get_mut("nextCursor").unwrap() = json!(encoded);
        assert_eq!(
            inventory(&first, binding(), None, 1).unwrap().next,
            Some(cursor)
        );
        let mut next = original.clone();
        *next.get_mut("ids").unwrap() = json!(["e04040404040404040404040404040404"]);
        assert!(inventory(&next, binding(), Some(cursor), 1).is_ok());
        assert!(inventory(&original, binding(), Some(cursor), 1).is_err());
        *next.get_mut("ids").unwrap() = json!(["e06060606060606060606060606060606"]);
        assert!(inventory(&next, binding(), Some(cursor), 1).is_err());
        *first.get_mut("ids").unwrap() = json!([]);
        assert!(inventory(&first, binding(), None, 1).is_err());
    }
}

pub const MAX_VALUES_PER_EMAIL: usize = 4096;

pub struct Email<'a> {
    pub id: EmailId,
    pub blob: crate::ids::BlobId,
    pub thread: crate::ids::ThreadId,
    pub size: u64,
    pub digest: [u8; 32],
    pub received_at: i64,
    pub mailboxes: Vec<crate::ids::MailboxId>,
    pub keywords: Vec<&'a str>,
}
pub struct Metadata<'a> {
    pub observed: DataState,
    pub emails: Vec<Email<'a>>,
    pub missing: Vec<EmailId>,
}
#[derive(Clone, Copy)]
pub struct Bounds {
    pub body_bytes: u64,
    pub values_per_email: usize,
}

pub fn metadata<'a>(
    value: &'a Json,
    binding: Binding,
    requested: &[EmailId],
    bounds: Bounds,
) -> Result<Metadata<'a>, Error> {
    if requested.len() > crate::postgres_body::MAX_METADATA_IDS
        || bounds.body_bytes > crate::postgres_body::MAX_BODY_BYTES
        || bounds.values_per_email == 0
        || bounds.values_per_email > MAX_VALUES_PER_EMAIL
    {
        return Err(Error::Capacity);
    }
    for (index, id) in requested.iter().enumerate() {
        if requested.get(..index).ok_or(Error::Invalid)?.contains(id) {
            return Err(Error::Invalid);
        }
    }
    fields(
        value,
        &["accountId", "sourceId", "state", "list", "notFound"],
    )?;
    let observed = scope(value, binding)?;
    let list = value
        .get("list")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    let not_found = value
        .get("notFound")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    if list.len().checked_add(not_found.len()) != Some(requested.len()) {
        return Err(Error::Invalid);
    }
    let mut seen = Vec::new();
    seen.try_reserve_exact(requested.len())
        .map_err(|_| Error::Capacity)?;
    let mut emails = Vec::new();
    emails
        .try_reserve_exact(list.len())
        .map_err(|_| Error::Capacity)?;
    let mut missing = Vec::new();
    missing
        .try_reserve_exact(not_found.len())
        .map_err(|_| Error::Capacity)?;
    for value in list {
        fields(
            value,
            &[
                "id",
                "blobId",
                "threadId",
                "size",
                "receivedAt",
                "mailboxIds",
                "keywords",
                "receivedAtMs",
                "bodySha256",
            ],
        )?;
        let id = local::<EmailId>(text(value, "id")?)?;
        if !requested.contains(&id) || seen.contains(&id) {
            return Err(Error::Invalid);
        }
        seen.push(id);
        let size = value
            .get("size")
            .and_then(Json::as_u64)
            .ok_or(Error::Invalid)?;
        if size > bounds.body_bytes {
            return Err(Error::Capacity);
        }
        let exact = text(value, "receivedAtMs")?;
        if exact.len() > 20 {
            return Err(Error::Invalid);
        }
        let received_at = exact.parse::<i64>().map_err(|_| Error::Invalid)?;
        let mut encoded = [0; 32];
        let mut out = crate::bounded::TextBuffer::new(&mut encoded);
        out.format(format_args!("{received_at}"))
            .map_err(|_| Error::Capacity)?;
        if out.as_str().map_err(|_| Error::Invalid)? != exact {
            return Err(Error::Invalid);
        }
        let mut date = [0; 24];
        let mut out = crate::bounded::TextBuffer::new(&mut date);
        crate::jmap_date::write_utc_ms(received_at, &mut out).map_err(|_| Error::Invalid)?;
        if out.as_str().map_err(|_| Error::Invalid)? != text(value, "receivedAt")? {
            return Err(Error::Invalid);
        }
        let digest =
            wire::unhex(text(value, "bodySha256")?.as_bytes()).map_err(|_| Error::Invalid)?;
        let mail = value
            .get("mailboxIds")
            .and_then(Json::as_object)
            .ok_or(Error::Invalid)?;
        let keys = value
            .get("keywords")
            .and_then(Json::as_object)
            .ok_or(Error::Invalid)?;
        if mail.is_empty() {
            return Err(Error::Invalid);
        }
        if mail.len() > bounds.values_per_email || keys.len() > bounds.values_per_email {
            return Err(Error::Capacity);
        }
        let mut mailboxes = Vec::new();
        mailboxes
            .try_reserve_exact(mail.len())
            .map_err(|_| Error::Capacity)?;
        for (name, flag) in mail {
            let mailbox = local::<crate::ids::MailboxId>(name)?;
            if flag.as_bool() != Some(true) {
                return Err(Error::Invalid);
            }
            mailboxes.push(mailbox);
        }
        mailboxes.sort_unstable();
        if mailboxes.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(Error::Invalid);
        }
        let mut keywords = Vec::new();
        keywords
            .try_reserve_exact(keys.len())
            .map_err(|_| Error::Capacity)?;
        for (name, flag) in keys {
            crate::format::key::Key::Keyword(id, name)
                .validate_local()
                .map_err(|_| Error::Invalid)?;
            if flag.as_bool() != Some(true) {
                return Err(Error::Invalid);
            }
            keywords.push(name.as_str());
        }
        keywords.sort_unstable();
        if keywords.windows(2).any(|pair| pair.first() == pair.get(1)) {
            return Err(Error::Invalid);
        }
        emails.push(Email {
            id,
            blob: local(text(value, "blobId")?)?,
            thread: local(text(value, "threadId")?)?,
            size,
            digest,
            received_at,
            mailboxes,
            keywords,
        });
    }
    for value in not_found {
        let id = local::<EmailId>(value.as_str().ok_or(Error::Invalid)?)?;
        if !requested.contains(&id) || seen.contains(&id) {
            return Err(Error::Invalid);
        }
        seen.push(id);
        missing.push(id);
    }
    Ok(Metadata {
        observed,
        emails,
        missing,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod metadata_tests {
    use super::*;
    use crate::format::Sequence;
    use td_json::json;
    fn binding() -> Binding {
        Binding {
            account: AccountId::from_bytes([1; 16]),
            epoch: StoreEpoch::from_bytes([2; 16]),
        }
    }
    fn bounds() -> Bounds {
        Bounds {
            body_bytes: 1024,
            values_per_email: 256,
        }
    }
    fn value() -> Json {
        let binding = binding();
        let state = DataState {
            account: binding.account,
            epoch: binding.epoch,
            kind: DataType::Email,
            sequence: Sequence::from_u64(7),
        };
        let mut bytes = [0; DataState::WIRE_BYTES];
        let state = state.encode(&mut bytes).unwrap();
        json!({"accountId":"a01010101010101010101010101010101","sourceId":"02020202020202020202020202020202","state":state,"notFound":[],"list":[{
            "id":"e03030303030303030303030303030303","blobId":"b04040404040404040404040404040404","threadId":"t05050505050505050505050505050505","size":100,
            "receivedAt":"1969-12-31T23:59:59.999Z","receivedAtMs":"-1",
            "bodySha256":"0606060606060606060606060606060606060606060606060606060606060606",
            "mailboxIds":{"m07070707070707070707070707070707":true},"keywords":{"$seen":true}
        }]})
    }
    #[test]
    fn exact_metadata_coverage_and_timestamp_are_preserved() {
        let value = value();
        let id = EmailId::from_bytes([3; 16]);
        let parsed = metadata(&value, binding(), &[id], bounds()).unwrap();
        let email = parsed.emails.first().unwrap();
        assert_eq!(email.id, id);
        assert_eq!(email.received_at, -1);
        assert_eq!(email.digest, [6; 32]);
        assert_eq!(email.keywords, vec!["$seen"]);
        assert!(parsed.missing.is_empty());
        assert!(metadata(&value, binding(), &[], bounds()).is_err());
        assert!(metadata(&value, binding(), &[id, id], bounds()).is_err());
        assert!(metadata(&value, binding(), &[EmailId::from_bytes([9; 16])], bounds()).is_err());
        assert!(matches!(
            metadata(
                &value,
                binding(),
                &[id],
                Bounds {
                    body_bytes: 99,
                    ..bounds()
                }
            ),
            Err(Error::Capacity)
        ));
    }
    #[test]
    fn metadata_rejects_contradictory_and_noncanonical_values() {
        let id = EmailId::from_bytes([3; 16]);
        for (name, replacement) in [
            ("receivedAtMs", json!("-0")),
            ("receivedAtMs", json!("+1")),
            ("receivedAt", json!("1970-01-01T00:00:00Z")),
            ("bodySha256", json!("0606")),
            ("size", json!(-1)),
            ("mailboxIds", json!({})),
            (
                "mailboxIds",
                json!({"m07070707070707070707070707070707":false}),
            ),
            ("keywords", json!({"$Seen":true})),
        ] {
            let mut value = value();
            let list = value.get("list").unwrap().as_array().unwrap();
            let mut email = list.first().unwrap().clone();
            *email.get_mut(name).unwrap() = replacement;
            *value.get_mut("list").unwrap() = json!([email]);
            assert!(
                metadata(&value, binding(), &[id], bounds()).is_err(),
                "{name}"
            );
        }
        let mut value = value();
        *value.get_mut("notFound").unwrap() = json!(["e03030303030303030303030303030303"]);
        assert!(metadata(&value, binding(), &[id], bounds()).is_err());
        *value.get_mut("list").unwrap() = json!([]);
        assert_eq!(
            metadata(&value, binding(), &[id], bounds())
                .unwrap()
                .missing,
            vec![id]
        );
    }
}

/// Match one requested method response before interpreting its payload.
pub fn reply<'a>(response: &'a Json, method: &str, call_id: &str) -> Result<&'a Json, Error> {
    let responses = response
        .get("methodResponses")
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    if responses.len() != 1 {
        return Err(Error::Invalid);
    }
    let call = responses
        .first()
        .and_then(Json::as_array)
        .ok_or(Error::Invalid)?;
    if call.len() != 3 || call.get(2).and_then(Json::as_str) != Some(call_id) {
        return Err(Error::Invalid);
    }
    let name = call.first().and_then(Json::as_str).ok_or(Error::Invalid)?;
    let result = call.get(1).ok_or(Error::Invalid)?;
    if name == "error" {
        return Err(match result.get("type").and_then(Json::as_str) {
            Some("cannotCalculateChanges") => Error::Reset,
            Some(_) => Error::RemoteFailure,
            None => Error::Invalid,
        });
    }
    if name != method || result.as_object().is_none() {
        return Err(Error::Invalid);
    }
    Ok(result)
}
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod response_tests {
    use super::*;
    use td_json::json;
    #[test]
    fn response_matches_method_and_tag_without_turning_errors_into_empty_data() {
        let good = json!({"methodResponses":[["TdReplica/inventory",{"ids":[]},"copy"]]});
        assert_eq!(
            reply(&good, "TdReplica/inventory", "copy").unwrap(),
            &json!({"ids":[]})
        );
        assert_eq!(
            reply(&good, "TdReplica/metadata", "copy"),
            Err(Error::Invalid)
        );
        assert_eq!(
            reply(&good, "TdReplica/inventory", "different"),
            Err(Error::Invalid)
        );
        let reset = json!({"methodResponses":[["error",{"type":"cannotCalculateChanges"},"copy"]]});
        assert_eq!(
            reply(&reset, "TdReplica/inventory", "copy"),
            Err(Error::Reset)
        );
        let refused = json!({"methodResponses":[["error",{"type":"serverFail"},"copy"]]});
        assert_eq!(
            reply(&refused, "TdReplica/inventory", "copy"),
            Err(Error::RemoteFailure)
        );
        for invalid in [
            json!({}),
            json!({"methodResponses":[]}),
            json!({"methodResponses":[["TdReplica/inventory",{},"copy"],["TdReplica/inventory",{},"copy"]]}),
        ] {
            assert_eq!(
                reply(&invalid, "TdReplica/inventory", "copy"),
                Err(Error::Invalid)
            );
        }
    }
}
