use crate::jmap::types::{Email, Mailbox};
use crate::json::{self, ToJson};
use crate::kv::{Error as KvError, Key, Store};
use std::path::{Path, PathBuf};

/// The five tables. `op_queue` is keyed by a big-endian `u64`, so iteration
/// order is numeric order and replay is FIFO; the rest are keyed by text.
const EMAILS: &str = "emails";
const RULES_PROCESSED: &str = "rules_processed";
const MAILBOX_INDEX: &str = "mailbox_index";
const MAILBOXES: &str = "mailboxes";
const OP_QUEUE: &str = "op_queue";

/// The single row `mailboxes` holds.
const MAILBOXES_ROW: &str = "mailboxes";

/// A cache value is one JSON document. Writing one cannot fail, so the
/// encoder returns bytes; a value that will not read back is a cache miss.
fn encode<T: ToJson + ?Sized>(value: &T) -> Vec<u8> {
    value.to_json().to_vec()
}

fn decode_email(bytes: &[u8]) -> Option<Email> {
    Email::from_json(&json::parse_slice(bytes).ok()?).ok()
}

fn decode_mailboxes(bytes: &[u8]) -> Option<Vec<Mailbox>> {
    let value = json::parse_slice(bytes).ok()?;
    value
        .as_arr()?
        .iter()
        .map(|m| Mailbox::from_json(m).ok())
        .collect()
}

fn decode_ids(bytes: &[u8]) -> Option<Vec<String>> {
    let value = json::parse_slice(bytes).ok()?;
    value
        .as_arr()?
        .iter()
        .map(|id| id.as_str().map(str::to_string))
        .collect()
}

pub struct Cache {
    store: Store,
}

fn cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("tmc")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache").join("tmc")
    } else {
        PathBuf::from("/tmp").join("tmc-cache")
    }
}

fn safe_account_name(account_name: &str) -> String {
    account_name.replace(['/', '\\', '\0'], "_")
}

fn db_path(account_name: &str) -> PathBuf {
    cache_dir().join(format!("{}.tdkv", safe_account_name(account_name)))
}

/// The redb file this store replaced. A different format under a different
/// name, so it is removed rather than read.
fn legacy_db_path(account_name: &str) -> PathBuf {
    cache_dir().join(format!("{}.redb", safe_account_name(account_name)))
}

impl Cache {
    pub fn open(account_name: &str) -> Result<Cache, String> {
        let path = db_path(account_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create cache dir: {}", e))?;
        }
        remove_legacy(&legacy_db_path(account_name));
        let store = match Store::open(&path) {
            Ok(store) => store,
            // The cache is derived from the server and rebuildable, so a log
            // recovery cannot explain throws it away and starts again rather
            // than leaving the account with no cache at all.
            Err(KvError::Corrupt(why)) => {
                log_warn!(
                    "[Cache] discarding corrupt cache at {}: {}",
                    path.display(),
                    why
                );
                std::fs::remove_file(&path).map_err(|e| {
                    format!("failed to remove corrupt cache {}: {}", path.display(), e)
                })?;
                Store::open(&path)
                    .map_err(|e| format!("failed to open cache db at {}: {}", path.display(), e))?
            }
            // Locked (a second tmc on the same account) and I/O errors both
            // reach the caller, which proceeds without a cache.
            Err(e) => {
                return Err(format!(
                    "failed to open cache db at {}: {}",
                    path.display(),
                    e
                ))
            }
        };
        Ok(Cache { store })
    }

    pub fn get_email(&self, id: &str) -> Option<Email> {
        let txn = self.store.read();
        let table = txn.table(EMAILS)?;
        decode_email(table.get(&Key::from_str(id))?)
    }

    #[allow(dead_code)]
    pub fn update_email_seen(&self, id: &str, seen: bool) {
        let _ = self.apply_mark_seen(id, seen);
    }

    #[allow(dead_code)]
    pub fn update_email_flagged(&self, id: &str, flagged: bool) {
        let _ = self.apply_set_flagged(id, flagged);
    }

    #[allow(dead_code)]
    pub fn remove_email(&self, id: &str) {
        let _ = self.apply_destroy_email(id);
    }

    pub fn put_emails(&self, emails: &[Email]) {
        if emails.is_empty() {
            return;
        }
        let mut txn = self.store.write();
        for email in emails {
            txn.insert(EMAILS, &Key::from_str(&email.id), &encode(email));
        }
        if let Err(e) = txn.commit() {
            log_warn!("[Cache] failed to commit emails: {}", e);
        }
    }

    pub fn get_mailbox_emails(&self, mailbox_id: &str) -> Option<Vec<Email>> {
        let txn = self.store.read();
        let index_table = txn.table(MAILBOX_INDEX)?;
        let email_ids = decode_ids(index_table.get(&Key::from_str(mailbox_id))?)?;
        if email_ids.is_empty() {
            return Some(Vec::new());
        }

        let email_table = txn.table(EMAILS)?;
        let mut emails = Vec::with_capacity(email_ids.len());
        for id in &email_ids {
            if let Some(bytes) = email_table.get(&Key::from_str(id)) {
                if let Some(email) = decode_email(bytes) {
                    emails.push(email);
                }
            }
        }
        if emails.is_empty() {
            None
        } else {
            Some(emails)
        }
    }

    pub fn put_mailbox_index(&self, mailbox_id: &str, email_ids: &[String]) {
        let mut txn = self.store.write();
        txn.insert(
            MAILBOX_INDEX,
            &Key::from_str(mailbox_id),
            &encode(email_ids),
        );
        if let Err(e) = txn.commit() {
            log_warn!("[Cache] failed to commit mailbox_index: {}", e);
        }
    }

    pub fn get_mailboxes(&self) -> Option<Vec<Mailbox>> {
        let txn = self.store.read();
        let table = txn.table(MAILBOXES)?;
        decode_mailboxes(table.get(&Key::from_str(MAILBOXES_ROW))?)
    }

    pub fn put_mailboxes(&self, mailboxes: &[Mailbox]) {
        let mut txn = self.store.write();
        txn.insert(MAILBOXES, &Key::from_str(MAILBOXES_ROW), &encode(mailboxes));
        if let Err(e) = txn.commit() {
            log_warn!("[Cache] failed to commit mailboxes: {}", e);
        }
    }

    pub fn get_thread_emails(&self, thread_id: &str) -> Vec<Email> {
        let txn = self.store.read();
        let Some(table) = txn.table(EMAILS) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (_, value) in table.iter() {
            if let Some(email) = decode_email(value) {
                if email.thread_id.as_deref() == Some(thread_id) {
                    out.push(email);
                }
            }
        }
        out
    }

    pub fn apply_mark_seen(&self, id: &str, seen: bool) -> bool {
        let mut txn = self.store.write();
        let key = Key::from_str(id);
        let Some(mut email) = txn.get(EMAILS, &key).and_then(decode_email) else {
            return false;
        };

        let was_seen = email.keywords.contains_key("$seen");
        if was_seen == seen {
            return false;
        }

        if seen {
            email.keywords.insert("$seen".to_string(), true);
        } else {
            email.keywords.remove("$seen");
        }

        let mailboxes_key = Key::from_str(MAILBOXES_ROW);
        let mut adjusted_mailboxes = txn
            .get(MAILBOXES, &mailboxes_key)
            .and_then(decode_mailboxes);

        if let Some(ref mut mailboxes) = adjusted_mailboxes {
            for mailbox in mailboxes {
                if email.mailbox_ids.contains_key(&mailbox.id) {
                    if seen {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_sub(1);
                    } else {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_add(1);
                    }
                }
            }
        }

        txn.insert(EMAILS, &key, &encode(&email));
        if let Some(mailboxes) = adjusted_mailboxes {
            txn.insert(MAILBOXES, &mailboxes_key, &encode(&mailboxes));
        }

        txn.commit().is_ok()
    }

    pub fn apply_set_flagged(&self, id: &str, flagged: bool) -> bool {
        let mut txn = self.store.write();
        let key = Key::from_str(id);
        let Some(mut email) = txn.get(EMAILS, &key).and_then(decode_email) else {
            return false;
        };

        let was_flagged = email.keywords.contains_key("$flagged");
        if was_flagged == flagged {
            return false;
        }
        if flagged {
            email.keywords.insert("$flagged".to_string(), true);
        } else {
            email.keywords.remove("$flagged");
        }
        txn.insert(EMAILS, &key, &encode(&email));
        txn.commit().is_ok()
    }

    pub fn apply_move_email(&self, id: &str, to_mailbox_id: &str) -> bool {
        let mut txn = self.store.write();
        let key = Key::from_str(id);
        let Some(mut email) = txn.get(EMAILS, &key).and_then(decode_email) else {
            return false;
        };
        let was_seen = email.keywords.contains_key("$seen");
        let previous_mailboxes: Vec<String> = email.mailbox_ids.keys().cloned().collect();

        email.mailbox_ids.clear();
        email.mailbox_ids.insert(to_mailbox_id.to_string(), true);

        txn.insert(EMAILS, &key, &encode(&email));

        for mailbox_id in &previous_mailboxes {
            let index_key = Key::from_str(mailbox_id);
            if let Some(mut ids) = txn.get(MAILBOX_INDEX, &index_key).and_then(decode_ids) {
                ids.retain(|eid| eid != id);
                txn.insert(MAILBOX_INDEX, &index_key, &encode(&ids));
            }
        }
        let target_key = Key::from_str(to_mailbox_id);
        let mut target_ids = txn
            .get(MAILBOX_INDEX, &target_key)
            .and_then(decode_ids)
            .unwrap_or_default();
        target_ids.retain(|eid| eid != id);
        target_ids.insert(0, id.to_string());
        txn.insert(MAILBOX_INDEX, &target_key, &encode(&target_ids));

        let mailboxes_key = Key::from_str(MAILBOXES_ROW);
        if let Some(mut mailboxes) = txn
            .get(MAILBOXES, &mailboxes_key)
            .and_then(decode_mailboxes)
        {
            for mailbox in &mut mailboxes {
                let was_in = previous_mailboxes.iter().any(|m| m == &mailbox.id);
                let now_in = mailbox.id == to_mailbox_id;
                if was_in && !now_in {
                    mailbox.total_emails = mailbox.total_emails.saturating_sub(1);
                    if !was_seen {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_sub(1);
                    }
                } else if !was_in && now_in {
                    mailbox.total_emails = mailbox.total_emails.saturating_add(1);
                    if !was_seen {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_add(1);
                    }
                }
            }
            txn.insert(MAILBOXES, &mailboxes_key, &encode(&mailboxes));
        }

        txn.commit().is_ok()
    }

    pub fn apply_destroy_email(&self, id: &str) -> bool {
        let mut txn = self.store.write();
        let key = Key::from_str(id);
        let Some(existing_email) = txn.get(EMAILS, &key).and_then(decode_email) else {
            return false;
        };

        let was_seen = existing_email.keywords.contains_key("$seen");
        let mailbox_ids: Vec<String> = existing_email.mailbox_ids.keys().cloned().collect();

        txn.remove(EMAILS, &key);

        for mailbox_id in &mailbox_ids {
            let index_key = Key::from_str(mailbox_id);
            if let Some(mut ids) = txn.get(MAILBOX_INDEX, &index_key).and_then(decode_ids) {
                ids.retain(|eid| eid != id);
                txn.insert(MAILBOX_INDEX, &index_key, &encode(&ids));
            }
        }

        let mailboxes_key = Key::from_str(MAILBOXES_ROW);
        if let Some(mut mailboxes) = txn
            .get(MAILBOXES, &mailboxes_key)
            .and_then(decode_mailboxes)
        {
            for mailbox in &mut mailboxes {
                if mailbox_ids.iter().any(|m| m == &mailbox.id) {
                    mailbox.total_emails = mailbox.total_emails.saturating_sub(1);
                    if !was_seen {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_sub(1);
                    }
                }
            }
            txn.insert(MAILBOXES, &mailboxes_key, &encode(&mailboxes));
        }

        txn.commit().is_ok()
    }

    pub fn apply_mark_mailbox_read(&self, mailbox_id: &str) -> usize {
        let mut txn = self.store.write();

        let mailbox_email_ids = txn
            .get(MAILBOX_INDEX, &Key::from_str(mailbox_id))
            .and_then(decode_ids)
            .unwrap_or_default();
        if mailbox_email_ids.is_empty() {
            return 0;
        }

        let mut changed = 0usize;
        let mut unread_delta_by_mailbox: std::collections::HashMap<String, u32> =
            std::collections::HashMap::new();
        for id in &mailbox_email_ids {
            let key = Key::from_str(id);
            let Some(mut email) = txn.get(EMAILS, &key).and_then(decode_email) else {
                continue;
            };
            if email.keywords.contains_key("$seen") {
                continue;
            }
            email.keywords.insert("$seen".to_string(), true);
            txn.insert(EMAILS, &key, &encode(&email));
            changed = changed.saturating_add(1);
            for mbx in email.mailbox_ids.keys() {
                *unread_delta_by_mailbox.entry(mbx.clone()).or_insert(0) += 1;
            }
        }

        if changed > 0 {
            let mailboxes_key = Key::from_str(MAILBOXES_ROW);
            if let Some(mut mailboxes) = txn
                .get(MAILBOXES, &mailboxes_key)
                .and_then(decode_mailboxes)
            {
                for mailbox in &mut mailboxes {
                    if let Some(delta) = unread_delta_by_mailbox.get(&mailbox.id) {
                        mailbox.unread_emails = mailbox.unread_emails.saturating_sub(*delta);
                    }
                }
                txn.insert(MAILBOXES, &mailboxes_key, &encode(&mailboxes));
            }
        }

        if txn.commit().is_err() {
            return 0;
        }
        changed
    }

    pub fn enqueue_operation(&self, payload: &[u8]) -> Result<u64, String> {
        let mut txn = self.store.write();
        // `last_key` is the greatest live key including this transaction's own
        // writes, so the sequence continues without a scan of the queue.
        let seq = txn
            .last_key(OP_QUEUE)
            .as_deref()
            .and_then(Key::as_u64)
            .unwrap_or(0)
            .saturating_add(1);
        txn.insert(OP_QUEUE, &Key::from_u64(seq), payload);
        txn.commit().map_err(|e| format!("queue commit: {}", e))?;
        Ok(seq)
    }

    pub fn queued_operations(&self) -> Vec<(u64, Vec<u8>)> {
        let txn = self.store.read();
        let Some(table) = txn.table(OP_QUEUE) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(table.len());
        for (key, value) in table.iter() {
            if let Some(seq) = Key::as_u64(key) {
                out.push((seq, value.to_vec()));
            }
        }
        out
    }

    pub fn remove_queued_operation(&self, seq: u64) -> bool {
        let mut txn = self.store.write();
        txn.remove(OP_QUEUE, &Key::from_u64(seq));
        txn.commit().is_ok()
    }

    pub fn filter_unprocessed(&self, ids: &[String]) -> Vec<String> {
        let txn = self.store.read();
        let Some(table) = txn.table(RULES_PROCESSED) else {
            return ids.to_vec();
        };
        ids.iter()
            .filter(|id| table.get(&Key::from_str(id)).is_none())
            .cloned()
            .collect()
    }

    pub fn mark_rules_processed(&self, ids: &[String]) {
        if ids.is_empty() {
            return;
        }
        let mut txn = self.store.write();
        for id in ids {
            txn.insert(RULES_PROCESSED, &Key::from_str(id), &[]);
        }
        if let Err(e) = txn.commit() {
            log_warn!("[Cache] failed to commit rules_processed: {}", e);
        }
    }

    pub fn clear_all_accounts() {
        let dir = cache_dir();
        if !dir.exists() {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                // `.redb` too: a store this build replaced is still a cache
                // file, and clearing the cache should take it away.
                let extension = path.extension().and_then(|e| e.to_str());
                if !matches!(extension, Some("tdkv") | Some("redb")) {
                    continue;
                }
                if let Err(e) = std::fs::remove_file(&path) {
                    eprintln!(
                        "Warning: failed to remove cache file {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }
}

/// Remove the redb file an earlier build left behind, saying so if it will
/// not go: it is dead weight in the cache directory, not an error.
fn remove_legacy(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => log_info!("[Cache] removed the redb cache at {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log_warn!(
            "[Cache] failed to remove the redb cache at {}: {}",
            path.display(),
            e
        ),
    }
}

#[cfg(test)]
impl Cache {
    fn is_rules_processed(&self, id: &str) -> bool {
        let txn = self.store.read();
        let Some(table) = txn.table(RULES_PROCESSED) else {
            return false;
        };
        table.get(&Key::from_str(id)).is_some()
    }

    fn clear(&self) {
        let keys: Vec<(&str, Vec<Vec<u8>>)> = {
            let txn = self.store.read();
            [EMAILS, RULES_PROCESSED, MAILBOX_INDEX, MAILBOXES, OP_QUEUE]
                .into_iter()
                .map(|name| {
                    let keys = txn
                        .table(name)
                        .map(|table| table.iter().map(|(key, _)| key.to_vec()).collect())
                        .unwrap_or_default();
                    (name, keys)
                })
                .collect()
        };
        let mut txn = self.store.write();
        for (name, keys) in &keys {
            for key in keys {
                txn.remove(name, key);
            }
        }
        let _ = txn.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_test_email(id: &str) -> Email {
        Email {
            id: id.to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: Some(format!("Test {}", id)),
            received_at: None,
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_cache_put_get() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_account").unwrap();

        let email = make_test_email("e1");
        cache.put_emails(std::slice::from_ref(&email));

        let cached = cache.get_email("e1").unwrap();
        assert_eq!(cached.id, "e1");
        assert_eq!(cached.subject.as_deref(), Some("Test e1"));

        assert!(cache.get_email("nonexistent").is_none());
    }

    #[test]
    fn test_cache_rules_processed() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_rules").unwrap();

        assert!(!cache.is_rules_processed("e1"));

        let unprocessed = cache.filter_unprocessed(&["e1".into(), "e2".into(), "e3".into()]);
        assert_eq!(unprocessed.len(), 3);

        cache.mark_rules_processed(&["e1".into(), "e2".into()]);

        assert!(cache.is_rules_processed("e1"));
        assert!(cache.is_rules_processed("e2"));
        assert!(!cache.is_rules_processed("e3"));

        let unprocessed = cache.filter_unprocessed(&["e1".into(), "e2".into(), "e3".into()]);
        assert_eq!(unprocessed, vec!["e3".to_string()]);
    }

    #[test]
    fn test_cache_mailboxes() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_mailboxes").unwrap();

        assert!(cache.get_mailboxes().is_none());

        let mailboxes = vec![
            Mailbox {
                id: "m1".into(),
                name: "Inbox".into(),
                parent_id: None,
                role: Some("inbox".into()),
                total_emails: 42,
                unread_emails: 3,
                sort_order: 1,
            },
            Mailbox {
                id: "m2".into(),
                name: "Sent".into(),
                parent_id: None,
                role: Some("sent".into()),
                total_emails: 100,
                unread_emails: 0,
                sort_order: 2,
            },
        ];
        cache.put_mailboxes(&mailboxes);

        let cached = cache.get_mailboxes().unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].id, "m1");
        assert_eq!(cached[0].name, "Inbox");
        assert_eq!(cached[0].unread_emails, 3);
        assert_eq!(cached[1].id, "m2");
        assert_eq!(cached[1].name, "Sent");
    }

    #[test]
    fn test_cache_mailbox_index() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_mbx_idx").unwrap();

        // No index yet
        assert!(cache.get_mailbox_emails("mbx1").is_none());

        // Store emails and index
        let e1 = make_test_email("e1");
        let e2 = make_test_email("e2");
        cache.put_emails(&[e1.clone(), e2.clone()]);
        cache.put_mailbox_index("mbx1", &["e1".into(), "e2".into()]);

        let cached = cache.get_mailbox_emails("mbx1").unwrap();
        assert_eq!(cached.len(), 2);
        assert_eq!(cached[0].id, "e1");
        assert_eq!(cached[1].id, "e2");

        // Index with missing email returns only found ones
        cache.put_mailbox_index("mbx2", &["e1".into(), "missing".into()]);
        let cached = cache.get_mailbox_emails("mbx2").unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, "e1");

        // Empty index returns empty vec
        cache.put_mailbox_index("mbx3", &[]);
        let cached = cache.get_mailbox_emails("mbx3").unwrap();
        assert!(cached.is_empty());
    }

    #[test]
    fn test_cache_clear() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_clear").unwrap();

        cache.put_emails(&[make_test_email("e1")]);
        cache.mark_rules_processed(&["e1".into()]);
        assert!(cache.get_email("e1").is_some());
        assert!(cache.is_rules_processed("e1"));

        cache.clear();
        assert!(cache.get_email("e1").is_none());
        assert!(!cache.is_rules_processed("e1"));
    }

    #[test]
    fn test_cache_update_email_seen() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_seen").unwrap();

        let email = make_test_email("e1");
        assert!(!email.keywords.contains_key("$seen"));
        cache.put_emails(&[email]);

        // Mark as seen
        cache.update_email_seen("e1", true);
        let cached = cache.get_email("e1").unwrap();
        assert!(cached.keywords.contains_key("$seen"));

        // Mark as unseen
        cache.update_email_seen("e1", false);
        let cached = cache.get_email("e1").unwrap();
        assert!(!cached.keywords.contains_key("$seen"));

        // No-op on missing email
        cache.update_email_seen("nonexistent", true);
        assert!(cache.get_email("nonexistent").is_none());
    }

    #[test]
    fn test_cache_update_email_flagged() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_flagged").unwrap();

        let email = make_test_email("e1");
        assert!(!email.keywords.contains_key("$flagged"));
        cache.put_emails(&[email]);

        // Flag
        cache.update_email_flagged("e1", true);
        let cached = cache.get_email("e1").unwrap();
        assert!(cached.keywords.contains_key("$flagged"));

        // Unflag
        cache.update_email_flagged("e1", false);
        let cached = cache.get_email("e1").unwrap();
        assert!(!cached.keywords.contains_key("$flagged"));
    }

    #[test]
    fn test_cache_move_and_destroy_updates_indexes_and_counts() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        let cache = Cache::open("test_move_destroy").unwrap();

        let mut e1 = make_test_email("e1");
        e1.mailbox_ids.insert("inbox".into(), true);
        let mut e2 = make_test_email("e2");
        e2.mailbox_ids.insert("inbox".into(), true);
        e2.keywords.insert("$seen".into(), true);
        cache.put_emails(&[e1, e2]);
        cache.put_mailbox_index("inbox", &["e1".into(), "e2".into()]);
        cache.put_mailbox_index("archive", &[]);
        cache.put_mailboxes(&[
            Mailbox {
                id: "inbox".into(),
                name: "INBOX".into(),
                parent_id: None,
                role: Some("inbox".into()),
                total_emails: 2,
                unread_emails: 1,
                sort_order: 0,
            },
            Mailbox {
                id: "archive".into(),
                name: "Archive".into(),
                parent_id: None,
                role: Some("archive".into()),
                total_emails: 0,
                unread_emails: 0,
                sort_order: 1,
            },
        ]);

        assert!(cache.apply_move_email("e1", "archive"));
        let mboxes = cache.get_mailboxes().unwrap();
        let inbox = mboxes.iter().find(|m| m.id == "inbox").unwrap();
        let archive = mboxes.iter().find(|m| m.id == "archive").unwrap();
        assert_eq!(inbox.total_emails, 1);
        assert_eq!(inbox.unread_emails, 0);
        assert_eq!(archive.total_emails, 1);
        assert_eq!(archive.unread_emails, 1);

        let inbox_ids: Vec<String> = cache
            .get_mailbox_emails("inbox")
            .unwrap_or_default()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        let archive_ids: Vec<String> = cache
            .get_mailbox_emails("archive")
            .unwrap_or_default()
            .iter()
            .map(|e| e.id.clone())
            .collect();
        assert_eq!(inbox_ids, vec!["e2".to_string()]);
        assert_eq!(archive_ids, vec!["e1".to_string()]);

        assert!(cache.apply_destroy_email("e1"));
        let mboxes = cache.get_mailboxes().unwrap();
        let archive = mboxes.iter().find(|m| m.id == "archive").unwrap();
        assert_eq!(archive.total_emails, 0);
        assert_eq!(archive.unread_emails, 0);
        assert!(cache.get_email("e1").is_none());
    }

    #[test]
    fn test_cache_queue_persistence() {
        let dir = crate::testing::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());

        let cache = Cache::open("test_queue").unwrap();
        let s1 = cache.enqueue_operation(br#"{"kind":"a"}"#).unwrap();
        let s2 = cache.enqueue_operation(br#"{"kind":"b"}"#).unwrap();
        assert!(s2 > s1);
        let ops = cache.queued_operations();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].0, s1);
        assert_eq!(ops[1].0, s2);
        assert!(cache.remove_queued_operation(s1));
        let ops = cache.queued_operations();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].0, s2);
    }
}
