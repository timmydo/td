//! Offline equivalence across independently assigned JMAP object IDs.
use super::*;
use std::collections::BTreeSet;

pub(super) fn run(left: &archive::Archive, right: &archive::Archive) -> Result<Json> {
    let a = left.snapshot()?;
    let b = right.snapshot()?;
    let left_inventory = inventory(&a)?;
    let right_inventory = inventory(&b)?;
    if left_inventory != right_inventory {
        return Err("migration-archives-differ");
    }
    let left_verified = left.verify()?;
    let right_verified = right.verify()?;
    Ok(json!({"format":1,"status":"archives-match",
        "leftSource":field(&a,"source")?,"rightSource":field(&b,"source")?,
        "leftManifestSha256":field(&left_verified,"manifestSha256")?,
        "rightManifestSha256":field(&right_verified,"manifestSha256")?,
        "messages":array(&a,"messages")?.len(),
        "mailboxes":array(&a,"mailboxes")?.len(),
        "messageBytes":field(&left_verified,"messageBytes")?,
        "comparison":"raw-digest-and-mail-metadata-multiset",
        "bodyDigestsVerified":true,"cutoverReady":false}))
}

#[derive(Eq, PartialEq)]
struct Inventory {
    folders: BTreeSet<String>,
    messages: BTreeMap<String, u64>,
}
fn inventory(manifest: &Json) -> Result<Inventory> {
    let mut pending: Vec<_> = array(manifest, "mailboxes")?.iter().collect();
    let mut paths = BTreeMap::<&str, String>::new();
    let mut unique = BTreeSet::new();
    let mut folders = BTreeSet::new();
    while !pending.is_empty() {
        let before = pending.len();
        let mut rest = Vec::new();
        for mailbox in pending {
            let parent = match field(mailbox, "parentId")? {
                Json::Null => None,
                Json::Str(id) => match paths.get(id.as_str()) {
                    Some(path) => Some(path),
                    None => {
                        rest.push(mailbox);
                        continue;
                    }
                },
                _ => return Err("migration-invalid-mailbox"),
            };
            // Hash length-delimited JSON components, never slash-joined names.
            // Parent fingerprints keep deep hierarchies within linear memory.
            let path = digest(
                json!([parent, string(mailbox, "name")?])
                    .to_string()
                    .as_bytes(),
            )?;
            if !unique.insert(path.clone()) {
                return Err("migration-ambiguous-folder-path");
            }
            let metadata = json!([
                path,
                field(mailbox, "role")?,
                integer(mailbox, "sortOrder")?,
                field(mailbox, "isSubscribed")?
            ]);
            folders.insert(digest(metadata.to_string().as_bytes())?);
            paths.insert(string(mailbox, "id")?, path);
        }
        if rest.len() == before {
            return Err("migration-missing-parent");
        }
        pending = rest;
    }
    let mut messages = BTreeMap::<String, u64>::new();
    for record in array(manifest, "messages")? {
        let email = field(record, "email")?;
        let mut memberships = Vec::new();
        for (id, _) in field(email, "mailboxIds")?
            .as_obj()
            .ok_or("migration-invalid-membership")?
        {
            memberships.push(paths.get(id.as_str()).ok_or("migration-missing-mailbox")?);
        }
        memberships.sort();
        let mut keywords = BTreeSet::new();
        for (keyword, _) in field(email, "keywords")?
            .as_obj()
            .ok_or("migration-invalid-keyword")?
        {
            if !keywords.insert(keyword.to_ascii_lowercase()) {
                return Err("migration-keyword-collision");
            }
        }
        let keywords: Vec<_> = keywords.into_iter().collect();
        let metadata = json!([
            string(record, "body")?,
            integer(email, "size")?,
            import::utc_ms(string(email, "receivedAt")?)?,
            memberships,
            keywords
        ]);
        let hash = digest(metadata.to_string().as_bytes())?;
        let count = messages.entry(hash).or_default();
        *count = count.checked_add(1).ok_or("migration-capacity")?;
    }
    Ok(Inventory { folders, messages })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn example(prefix: &str, date: &str) -> Json {
        let inbox = format!("{prefix}inbox");
        let child = format!("{prefix}child");
        let email = |id| {
            json!({"email":{"id":format!("{prefix}{id}"),
            "blobId":format!("{prefix}blob"),"size":3,"receivedAt":date,
            "mailboxIds":{inbox.clone():true,child.clone():true},
            "keywords":{"$seen":true,"custom":true}},"body":"a".repeat(64)})
        };
        json!({"version":1,"provisional":true,"coverage":"per-mailbox-query",
            "source":null,"mailboxState":"m","emailState":"e",
            "mailboxes":[
                {"id":inbox,"name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},
                {"id":child,"name":"Child","parentId":inbox,"role":null,"sortOrder":2,"isSubscribed":false}],
            "messages":[email("one"),email("two")]})
    }
    fn set(value: &mut Json, key: &str, replacement: Json) {
        if let Json::Obj(fields) = value {
            fields.iter_mut().find(|(name, _)| name == key).unwrap().1 = replacement;
        }
    }
    fn change_message(value: &Json, key: &str, replacement: Json, record: bool) -> Json {
        let mut changed = value.clone();
        let mut records = array(value, "messages").unwrap().to_vec();
        let first = records.first_mut().unwrap();
        if record {
            set(first, key, replacement);
        } else {
            let mut email = field(first, "email").unwrap().clone();
            set(&mut email, key, replacement);
            set(first, "email", email);
        }
        set(&mut changed, "messages", json!(records));
        changed
    }
    #[test]
    fn identifiers_order_and_exact_date_spelling_do_not_change_equivalence() {
        let a = example("a", "2024-01-02T03:04:05.100Z");
        let mut b = example("b", "2024-01-02T03:04:05.1Z");
        for key in ["mailboxes", "messages"] {
            let mut list = array(&b, key).unwrap().to_vec();
            list.reverse();
            set(&mut b, key, json!(list));
        }
        validate_manifest(&a).unwrap();
        validate_manifest(&b).unwrap();
        assert!(inventory(&a).unwrap() == inventory(&b).unwrap());
        assert_eq!(
            inventory(&a)
                .unwrap()
                .messages
                .values()
                .copied()
                .sum::<u64>(),
            2
        );
    }
    #[test]
    fn every_retained_message_property_and_duplicate_count_matters() {
        let a = example("a", "2024-01-02T03:04:05.100Z");
        for (key, value, record) in [
            ("body", json!("b".repeat(64)), true),
            ("size", json!(4), false),
            ("receivedAt", json!("2024-01-02T03:04:05.101Z"), false),
            ("mailboxIds", json!({"ainbox":true}), false),
            ("keywords", json!({"$flagged":true}), false),
        ] {
            let b = change_message(&a, key, value, record);
            assert!(inventory(&a).unwrap() != inventory(&b).unwrap(), "{key}");
        }
        let mut b = a.clone();
        set(
            &mut b,
            "messages",
            json!([array(&a, "messages").unwrap().first().unwrap()]),
        );
        assert!(inventory(&a).unwrap() != inventory(&b).unwrap());
        let b = change_message(&a, "keywords", json!({"Seen":true,"seen":true}), false);
        assert!(matches!(inventory(&b), Err("migration-keyword-collision")));
        let b = change_message(&a, "receivedAt", json!("2024-01-02T03:04:05.1001Z"), false);
        assert!(matches!(
            inventory(&b),
            Err("migration-unrepresentable-date")
        ));
    }
    #[test]
    fn hierarchy_empty_folders_and_every_folder_property_matter() {
        let a = example("a", "2024-01-02T03:04:05Z");
        for (key, value) in [
            ("name", json!("Renamed")),
            ("parentId", Json::Null),
            ("role", json!("archive")),
            ("sortOrder", json!(3)),
            ("isSubscribed", json!(true)),
        ] {
            let mut b = a.clone();
            let mut boxes = array(&a, "mailboxes").unwrap().to_vec();
            set(boxes.get_mut(1).unwrap(), key, value);
            set(&mut b, "mailboxes", json!(boxes));
            assert!(inventory(&a).unwrap() != inventory(&b).unwrap(), "{key}");
        }
        let mut b = a.clone();
        let mut boxes = array(&a, "mailboxes").unwrap().to_vec();
        let mut extra = boxes.first().unwrap().clone();
        set(&mut extra, "id", json!("extra"));
        let mut distinct = extra.clone();
        set(&mut distinct, "name", json!("Empty"));
        set(&mut distinct, "role", Json::Null);
        let mut with_empty = boxes.clone();
        with_empty.push(distinct);
        set(&mut b, "mailboxes", json!(with_empty));
        assert!(inventory(&a).unwrap() != inventory(&b).unwrap());
        boxes.push(extra);
        set(&mut b, "mailboxes", json!(boxes));
        assert!(matches!(
            inventory(&b),
            Err("migration-ambiguous-folder-path")
        ));
    }
}
