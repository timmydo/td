//! Passive Session negotiation for a pinned td-mta source origin.
use super::{local, Error};
use crate::{
    bounded::TextBuffer,
    config::endpoint::{HttpsUri, Origin, MAX_URI_BYTES},
    ids::{AccountId, BlobId},
    jmap_request::{CORE, MAIL, REPLICATION},
    wire,
};
use td_json::Json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceLimits {
    pub inventory_ids: usize,
    pub metadata_ids: usize,
    pub request_bytes: usize,
}

/// Validated routing metadata, not authentication or replica publication authority.
/// The response owner keeps all borrowed strings alive and immutable.
pub struct Discovery<'a> {
    origin: Origin<'a>,
    account: AccountId,
    api: &'a str,
    download: &'a str,
    limits: SourceLimits,
}
impl Discovery<'_> {
    pub fn account(&self) -> AccountId {
        self.account
    }
    pub fn api_url(&self) -> &str {
        self.api
    }
    pub fn limits(&self) -> SourceLimits {
        self.limits
    }
    pub fn download_url(&self, blob: BlobId) -> Result<String, Error> {
        let mut account = [0; wire::IdKind::WIRE_BYTES];
        let account = wire::encode_id(self.account, &mut account).map_err(|_| Error::Invalid)?;
        let mut blob_text = [0; wire::IdKind::WIRE_BYTES];
        let blob = wire::encode_id(blob, &mut blob_text).map_err(|_| Error::Invalid)?;
        let mut bytes = [0; MAX_URI_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        let mut rest = self.download;
        let mut seen = 0u8;
        while let Some(start) = rest.find('{') {
            out.append(rest.get(..start).ok_or(Error::Invalid)?)
                .map_err(|_| Error::Capacity)?;
            rest = rest.get(start + 1..).ok_or(Error::Invalid)?;
            let end = rest.find('}').ok_or(Error::Invalid)?;
            let (bit, replacement) = match rest.get(..end).ok_or(Error::Invalid)? {
                "accountId" => (1, account),
                "blobId" => (2, blob),
                "name" => (4, "message.eml"),
                "type" => (8, "message%2Frfc822"),
                _ => return Err(Error::Invalid),
            };
            if seen & bit != 0 {
                return Err(Error::Invalid);
            }
            seen |= bit;
            out.append(replacement).map_err(|_| Error::Capacity)?;
            rest = rest.get(end + 1..).ok_or(Error::Invalid)?;
        }
        out.append(rest).map_err(|_| Error::Capacity)?;
        if seen != 15 {
            return Err(Error::Invalid);
        }
        let text = out.as_str().map_err(|_| Error::Invalid)?;
        endpoint(self.origin, text)?;
        let mut result = String::new();
        result
            .try_reserve_exact(text.len())
            .map_err(|_| Error::Capacity)?;
        result.push_str(text);
        Ok(result)
    }
}
fn object(value: &Json) -> Result<&[(String, Json)], Error> {
    let fields = value.as_object().ok_or(Error::Invalid)?;
    if fields.len() > 64 {
        return Err(Error::Capacity);
    }
    Ok(fields)
}
fn member<'a>(value: &'a Json, name: &str) -> Result<Option<&'a Json>, Error> {
    let mut found = None;
    for (key, value) in object(value)? {
        if key == name {
            if found.is_some() {
                return Err(Error::Invalid);
            }
            found = Some(value);
        }
    }
    Ok(found)
}
fn required<'a>(value: &'a Json, name: &str) -> Result<&'a Json, Error> {
    member(value, name)?.ok_or(Error::Invalid)
}
fn string<'a>(value: &'a Json, name: &str) -> Result<&'a str, Error> {
    required(value, name)?.as_str().ok_or(Error::Invalid)
}
fn capability<'a>(value: &'a Json, name: &str) -> Result<&'a Json, Error> {
    let value = member(value, name)?.ok_or(Error::Unsupported)?;
    object(value)?;
    Ok(value)
}
fn limit(value: &Json, name: &str, local: usize) -> Result<usize, Error> {
    let value = required(value, name)?
        .as_u64()
        .filter(|n| *n != 0)
        .ok_or(Error::Invalid)?;
    Ok(usize::try_from(value).unwrap_or(usize::MAX).min(local))
}
fn endpoint(origin: Origin<'_>, value: &str) -> Result<(), Error> {
    let uri = HttpsUri::parse(value).map_err(|_| Error::Invalid)?;
    if !origin.same_origin(uri.origin()) {
        return Err(Error::SourceChanged);
    }
    Ok(())
}

/// Select only the requested account, or the sole account on initial discovery.
/// Additional Session fields/capabilities are ignored; relevant duplicates refuse.
/// The caller must bound JSON input/allocation before constructing this value.
pub fn discover<'a>(
    value: &'a Json,
    origin: &'a str,
    expected: Option<AccountId>,
) -> Result<Discovery<'a>, Error> {
    let origin = Origin::parse(origin).map_err(|_| Error::Invalid)?;
    let caps = required(value, "capabilities")?;
    let core = capability(caps, CORE)?;
    capability(caps, MAIL)?;
    capability(caps, REPLICATION)?;
    limit(core, "maxCallsInRequest", 1)?;
    let request_bytes = limit(core, "maxSizeRequest", 1024 * 1024)?;
    let accounts = required(value, "accounts")?;
    let (account, selected) = if let Some(account) = expected {
        let mut encoded = [0; wire::IdKind::WIRE_BYTES];
        let id = wire::encode_id(account, &mut encoded).map_err(|_| Error::Invalid)?;
        (account, member(accounts, id)?.ok_or(Error::SourceChanged)?)
    } else {
        let entries = object(accounts)?;
        if entries.len() != 1 {
            return Err(Error::Invalid);
        }
        let (id, selected) = entries.first().ok_or(Error::Invalid)?;
        (local::<AccountId>(id)?, selected)
    };
    let caps = required(selected, "accountCapabilities")?;
    capability(caps, MAIL)?;
    let replica = capability(caps, REPLICATION)?;
    let limits = SourceLimits {
        request_bytes,
        inventory_ids: limit(
            replica,
            "maxInventoryIds",
            crate::postgres_read::MAX_REPLICA_INVENTORY_PAGE,
        )?,
        metadata_ids: limit(
            replica,
            "maxMetadataIds",
            crate::postgres_body::MAX_METADATA_IDS,
        )?,
    };
    let api = string(value, "apiUrl")?;
    endpoint(origin, api)?;
    let download = string(value, "downloadUrl")?;
    if download.len() > MAX_URI_BYTES {
        return Err(Error::Capacity);
    }
    let result = Discovery {
        origin,
        account,
        api,
        download,
        limits,
    };
    // Validate every placeholder and the expanded origin before retaining it.
    result.download_url(BlobId::from_bytes([0; 16]))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use td_json::json;
    const ORIGIN: &str = "https://source.example";
    const DOWNLOAD: &str =
        "https://source.example/jmap/download/{accountId}/{blobId}/{name}?type={type}";
    fn account() -> AccountId {
        AccountId::from_bytes([1; 16])
    }
    fn encoded<T: wire::LocalId>(id: T) -> String {
        let mut out = [0; wire::IdKind::WIRE_BYTES];
        wire::encode_id(id, &mut out).unwrap().to_owned()
    }
    fn value() -> Json {
        let caps = Json::Obj(vec![
            (
                CORE.to_owned(),
                json!({"maxSizeRequest":1048576,"maxCallsInRequest":16}),
            ),
            (MAIL.to_owned(), json!({})),
            (REPLICATION.to_owned(), json!({})),
        ]);
        let account_caps = Json::Obj(vec![
            (MAIL.to_owned(), json!({})),
            (
                REPLICATION.to_owned(),
                json!({"maxInventoryIds":256,"maxMetadataIds":16}),
            ),
        ]);
        json!({"capabilities":caps,"accounts":Json::Obj(vec![(encoded(account()),json!({"name":"source","accountCapabilities":account_caps}))]),
            "apiUrl":"https://source.example/jmap/api","downloadUrl":DOWNLOAD,"futureField":true})
    }
    fn selected(value: &mut Json) -> &mut Json {
        value
            .get_mut("accounts")
            .unwrap()
            .get_mut(&encoded(account()))
            .unwrap()
    }
    fn limits(value: &mut Json) -> &mut Json {
        selected(value)
            .get_mut("accountCapabilities")
            .unwrap()
            .get_mut(REPLICATION)
            .unwrap()
    }
    #[test]
    fn source_session_selects_the_bound_account_and_expands_only_typed_ids() {
        let value = value();
        let plan = discover(&value, ORIGIN, None).unwrap();
        assert_eq!(plan.account(), account());
        assert_eq!(plan.api_url(), "https://source.example/jmap/api");
        assert_eq!(
            plan.limits(),
            SourceLimits {
                inventory_ids: 256,
                metadata_ids: 16,
                request_bytes: 1048576
            }
        );
        let blob = BlobId::from_bytes([7; 16]);
        assert_eq!(
            plan.download_url(blob).unwrap(),
            format!(
                "{ORIGIN}/jmap/download/{}/{}/message.eml?type=message%2Frfc822",
                encoded(account()),
                encoded(blob)
            )
        );
        assert!(discover(&value, "https://SOURCE.EXAMPLE:443/", Some(account())).is_ok());
        assert_eq!(
            discover(&value, ORIGIN, Some(AccountId::from_bytes([2; 16]))).err(),
            Some(Error::SourceChanged)
        );
        let mut multi = value.clone();
        let extra = selected(&mut multi).clone();
        let _ = multi
            .get_mut("accounts")
            .unwrap()
            .insert(encoded(AccountId::from_bytes([2; 16])), extra);
        assert!(discover(&multi, ORIGIN, None).is_err());
        assert_eq!(
            discover(&multi, ORIGIN, Some(account())).unwrap().account(),
            account()
        );
    }
    #[test]
    fn missing_capabilities_and_invalid_limits_never_become_default_support() {
        let mut missing = value();
        let _ = missing.insert("capabilities", json!({}));
        assert_eq!(
            discover(&missing, ORIGIN, None).err(),
            Some(Error::Unsupported)
        );
        let mut malformed = value();
        let _ = malformed
            .get_mut("capabilities")
            .unwrap()
            .insert(REPLICATION, Json::Null);
        assert_eq!(
            discover(&malformed, ORIGIN, None).err(),
            Some(Error::Invalid)
        );
        for field in ["maxInventoryIds", "maxMetadataIds"] {
            for invalid in [json!(0), json!(-1), Json::Null, json!("16"), json!(1.5)] {
                let mut value = value();
                let _ = limits(&mut value).insert(field, invalid);
                assert_eq!(discover(&value, ORIGIN, None).err(), Some(Error::Invalid));
            }
        }
        let mut larger = value();
        let _ = limits(&mut larger).insert("maxMetadataIds", json!(10000));
        assert_eq!(
            discover(&larger, ORIGIN, None)
                .unwrap()
                .limits()
                .metadata_ids,
            16
        );
        let mut smaller = value();
        let _ = limits(&mut smaller).insert("maxInventoryIds", json!(7));
        assert_eq!(
            discover(&smaller, ORIGIN, None)
                .unwrap()
                .limits()
                .inventory_ids,
            7
        );
    }
    #[test]
    fn discovered_urls_never_widen_origin_or_accept_ambiguous_templates() {
        for api in [
            "http://source.example/jmap/api",
            "https://other.example/jmap/api",
            "https://source.example:444/jmap/api",
            "https://user:pass@source.example/jmap/api",
            "/jmap/api",
            "https://source.example/jmap/api#fragment",
        ] {
            let mut value = value();
            let _ = value.insert("apiUrl", json!(api));
            assert!(discover(&value, ORIGIN, None).is_err(), "{api}");
        }
        for template in [
            DOWNLOAD.replace("source.example", "other.example"),
            DOWNLOAD.replace("{blobId}", "fixed"),
            DOWNLOAD.replace("{blobId}", "{blobId}{blobId}"),
            DOWNLOAD.replace("{name}", "{unknown}"),
            DOWNLOAD.replace("{type}", "{+type}"),
            DOWNLOAD.replace("{name}", "{name"),
            format!("{DOWNLOAD}}}"),
            format!("{DOWNLOAD}{}", "x".repeat(MAX_URI_BYTES)),
        ] {
            let mut value = value();
            let _ = value.insert("downloadUrl", json!(template));
            assert!(discover(&value, ORIGIN, None).is_err());
        }
        assert!(discover(&value(), "https://source.example/path", None).is_err());
    }
    #[test]
    fn relevant_duplicate_fields_are_rejected_before_selection() {
        let mut value = value();
        let Json::Obj(fields) = &mut value else {
            panic!("object fixture")
        };
        fields.push(("apiUrl".to_owned(), json!("https://other.example")));
        assert_eq!(discover(&value, ORIGIN, None).err(), Some(Error::Invalid));
        let mut value = self::value();
        let Json::Obj(fields) = limits(&mut value) else {
            panic!("limits fixture")
        };
        fields.push(("maxMetadataIds".to_owned(), json!(1)));
        assert_eq!(discover(&value, ORIGIN, None).err(), Some(Error::Invalid));
    }
    #[test]
    fn actual_td_mta_session_documents_negotiate_both_supported_profiles() {
        use crate::{
            config::identities::AccountView,
            jmap_session::{Concurrency, Session},
            limits::Limits,
        };
        let plan = Limits::default().plan().unwrap();
        for sending in [false, true] {
            let account = AccountView {
                id: account(),
                username: "replica source",
                name: "source",
            };
            let concurrency = Concurrency {
                uploads: 1,
                requests: 1,
            };
            let session = if sending {
                Session::mail_submission(
                    Origin::parse(ORIGIN).unwrap(),
                    account,
                    &plan,
                    concurrency,
                )
            } else {
                Session::mail_access(Origin::parse(ORIGIN).unwrap(), account, &plan, concurrency)
            }
            .unwrap();
            let json = td_json::parse_slice(session.bytes()).unwrap();
            let discovery = discover(&json, ORIGIN, Some(account.id)).unwrap();
            assert_eq!(discovery.account(), account.id);
            assert_eq!(
                discovery.limits().inventory_ids,
                plan.limits().query_page.min(256)
            );
            assert_eq!(
                discovery.limits().metadata_ids,
                plan.limits().objects_per_method.min(16)
            );
            assert!(discovery
                .download_url(BlobId::from_bytes([3; 16]))
                .unwrap()
                .ends_with("/message.eml?type=message%2Frfc822"));
        }
    }
}
