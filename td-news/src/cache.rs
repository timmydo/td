use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::feed::{Article, FeedMeta};
use crate::json::{self, Json, ToJson};
use crate::kv::{self, Key, Store, WriteTxn};

const ARTICLES: &str = "articles";
const FEEDS: &str = "feeds";
const FEED_INDEX: &str = "feed_index";

/// The cache file. A new name, because the store is a different format:
/// a redb file left at the old name would be read as a damaged log, and
/// the point of the rename is that it is never opened at all.
const CACHE_FILE: &str = "cache.tdkv";

/// What tn kept before this: a redb database nothing here can read.
const LEGACY_CACHE_FILE: &str = "tn.redb";

pub struct Cache {
    store: Arc<Store>,
}

/// A feed's article hashes, as the array the index table holds.
fn hashes_to_json(hashes: &[String]) -> Json {
    Json::Arr(hashes.iter().map(|hash| Json::Str(hash.clone())).collect())
}

/// The reader for [`hashes_to_json`]; `None` for anything but an array of
/// strings.
fn hashes_from_json(value: &Json) -> Option<Vec<String>> {
    value
        .as_arr()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect()
}

pub struct FeedRefresh {
    pub url: String,
    pub name: String,
    pub title: String,
    pub fetched_at: String,
    pub articles: Vec<Article>,
}

pub struct FeedRefreshSummary {
    pub feed_url: String,
    pub feed_name: String,
    pub fetched_at: String,
    pub new_articles: usize,
    pub total: usize,
    pub unread: usize,
}

impl Cache {
    pub fn open() -> Result<Cache, String> {
        remove_legacy_cache();
        let path = Self::default_db_path();
        Self::open_at(path)
    }

    pub fn open_at(path: PathBuf) -> Result<Cache, String> {
        let (store, replaced) = open_store(&path)?;
        if let Some(why) = replaced {
            crate::log::warn(format!(
                "cache {} unreadable ({}); starting an empty one",
                path.display(),
                why
            ));
        }
        Ok(Cache {
            store: Arc::new(store),
        })
    }

    pub fn default_db_path() -> PathBuf {
        cache_dir().join(CACHE_FILE)
    }

    pub fn clear() {
        remove_legacy_cache();
        let path = Self::default_db_path();
        Self::clear_at(path);
    }

    pub fn clear_at(path: PathBuf) {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        // The store's compaction scratch file, left behind only by a crash
        // mid-compaction; an open would delete it, and a clear should too.
        let mut scratch = path.into_os_string();
        scratch.push(".compact");
        let scratch = PathBuf::from(scratch);
        if scratch.exists() {
            let _ = std::fs::remove_file(&scratch);
        }
    }

    pub fn get_article(&self, hash: &str) -> Option<Article> {
        let txn = self.store.read();
        let table = txn.table(ARTICLES)?;
        let value = table.get(&Key::from_str(hash))?;
        Article::from_json(&json::parse_slice(value).ok()?)
    }

    pub fn put_articles(&self, articles: &[Article]) {
        let mut txn = self.store.write();
        for article in articles {
            insert_json(&mut txn, ARTICLES, &article.hash, &article.to_json());
        }
        commit(txn, "put_articles");
    }

    pub fn get_feed_meta(&self, url: &str) -> Option<FeedMeta> {
        let txn = self.store.read();
        let table = txn.table(FEEDS)?;
        let value = table.get(&Key::from_str(url))?;
        FeedMeta::from_json(&json::parse_slice(value).ok()?)
    }

    #[cfg(test)]
    pub fn put_feed_meta(&self, meta: &FeedMeta) {
        let mut txn = self.store.write();
        insert_json(&mut txn, FEEDS, &meta.url, &meta.to_json());
        commit(txn, "put_feed_meta");
    }

    pub fn get_feed_index(&self, feed_url: &str) -> Option<Vec<String>> {
        let txn = self.store.read();
        let table = txn.table(FEED_INDEX)?;
        let value = table.get(&Key::from_str(feed_url))?;
        hashes_from_json(&json::parse_slice(value).ok()?)
    }

    #[cfg(test)]
    pub fn put_feed_index(&self, feed_url: &str, hashes: &[String]) {
        let mut txn = self.store.write();
        insert_json(&mut txn, FEED_INDEX, feed_url, &hashes_to_json(hashes));
        commit(txn, "put_feed_index");
    }

    /// Every feed's new articles, metadata and index, in one transaction:
    /// the three tables move together or not at all.
    pub fn apply_refresh_batch(&self, refreshes: &[FeedRefresh]) -> Vec<FeedRefreshSummary> {
        if refreshes.is_empty() {
            return Vec::new();
        }

        let mut txn = self.store.write();
        let mut new_hashes_by_url = HashMap::<String, Vec<String>>::new();

        for refresh in refreshes {
            for article in &refresh.articles {
                // The transaction reads through its own writes, so an
                // article this batch has already staged counts as present
                // and its read state is not reset by a second feed
                // carrying it.
                if txn.get(ARTICLES, &Key::from_str(&article.hash)).is_some() {
                    continue;
                }
                insert_json(&mut txn, ARTICLES, &article.hash, &article.to_json());
                new_hashes_by_url
                    .entry(refresh.url.clone())
                    .or_default()
                    .push(article.hash.clone());
            }
        }

        for refresh in refreshes {
            let meta = FeedMeta {
                url: refresh.url.clone(),
                title: refresh.title.clone(),
                last_fetched: refresh.fetched_at.clone(),
            };
            insert_json(&mut txn, FEEDS, &meta.url, &meta.to_json());
        }

        let mut hashes_by_url = HashMap::<String, Vec<String>>::new();
        for refresh in refreshes {
            let mut hashes: Vec<String> = txn
                .get(FEED_INDEX, &Key::from_str(&refresh.url))
                .and_then(|value| json::parse_slice(value).ok())
                .and_then(|value| hashes_from_json(&value))
                .unwrap_or_default();

            if let Some(new_hashes) = new_hashes_by_url.get(&refresh.url) {
                for hash in new_hashes {
                    if !hashes.contains(hash) {
                        hashes.insert(0, hash.clone());
                    }
                }
            }

            insert_json(&mut txn, FEED_INDEX, &refresh.url, &hashes_to_json(&hashes));
            hashes_by_url.insert(refresh.url.clone(), hashes);
        }

        let mut summaries = Vec::with_capacity(refreshes.len());
        for refresh in refreshes {
            let hashes = hashes_by_url.get(&refresh.url).cloned().unwrap_or_default();
            let unread = hashes
                .iter()
                .filter(|hash| {
                    txn.get(ARTICLES, &Key::from_str(hash))
                        .and_then(|value| json::parse_slice(value).ok())
                        .and_then(|value| Article::from_json(&value))
                        .map(|article| !article.read)
                        .unwrap_or(false)
                })
                .count();
            summaries.push(FeedRefreshSummary {
                feed_url: refresh.url.clone(),
                feed_name: refresh.name.clone(),
                fetched_at: refresh.fetched_at.clone(),
                new_articles: new_hashes_by_url
                    .get(&refresh.url)
                    .map(|hashes| hashes.len())
                    .unwrap_or(0),
                total: hashes.len(),
                unread,
            });
        }

        // Nothing was stored if the commit did not take, so nothing is
        // reported as stored either.
        if !commit(txn, "apply_refresh_batch") {
            return Vec::new();
        }
        summaries
    }

    pub fn mark_read(&self, hash: &str, read: bool) {
        if let Some(mut article) = self.get_article(hash) {
            article.read = read;
            self.put_articles(&[article]);
        }
    }

    pub fn list_feed_urls(&self) -> Vec<String> {
        let mut urls = Vec::<String>::new();
        let txn = self.store.read();
        for name in [FEED_INDEX, FEEDS] {
            if let Some(table) = txn.table(name) {
                for (key, _) in table.iter() {
                    if let Some(url) = Key::as_str(key) {
                        urls.push(url.to_string());
                    }
                }
            }
        }

        urls.sort();
        urls.dedup();
        urls
    }

    pub fn list_feed_articles(&self, feed_url: &str) -> Vec<Article> {
        self.get_feed_index(feed_url)
            .unwrap_or_default()
            .iter()
            .filter_map(|hash| self.get_article(hash))
            .collect()
    }

    pub fn list_all_articles(&self) -> Vec<Article> {
        let mut articles = Vec::new();
        for url in self.list_feed_urls() {
            articles.extend(self.list_feed_articles(&url));
        }
        articles
    }

    pub fn list_unread_articles(&self) -> Vec<Article> {
        self.list_all_articles()
            .into_iter()
            .filter(|a| !a.read)
            .collect()
    }
}

/// Open the store, and say why it had to replace the file if it did.
///
/// A crash leaves a short tail, which the store repairs itself; anything
/// else is damage, or another format entirely (the redb cache, under a name
/// someone passed on the command line). The cache is derived data, so an
/// unreadable one is thrown away rather than reported to a person who could
/// do nothing with the news. Split from `open_at` because the caller is what
/// logs, and a test wants the answer rather than a log line.
fn open_store(path: &Path) -> Result<(Store, Option<String>), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create cache dir: {}", e))?;
    }
    match Store::open(path) {
        Ok(store) => Ok((store, None)),
        Err(kv::Error::Corrupt(why)) => {
            std::fs::remove_file(path)
                .map_err(|e| format!("remove unreadable cache {}: {}", path.display(), e))?;
            let store =
                Store::open(path).map_err(|e| format!("open cache {}: {}", path.display(), e))?;
            Ok((store, Some(why)))
        }
        Err(e) => Err(format!("open cache {}: {}", path.display(), e)),
    }
}

fn cache_dir() -> PathBuf {
    let xdg = std::env::var("XDG_CACHE_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.cache", home)
    });
    PathBuf::from(xdg).join("tn")
}

/// Delete the redb cache tn used to keep. Nothing reads it now, and it
/// holds a copy of every article that was ever fetched.
fn remove_legacy_cache() {
    remove_legacy_cache_in(&cache_dir());
}

fn remove_legacy_cache_in(dir: &Path) {
    let legacy = dir.join(LEGACY_CACHE_FILE);
    if legacy.exists() {
        let _ = std::fs::remove_file(&legacy);
    }
}

fn insert_json(txn: &mut WriteTxn<'_>, table: &str, key: &str, value: &Json) {
    txn.insert(table, &Key::from_str(key), &value.to_vec());
}

/// Commit, and say whether it took. A cache write has no one to report to
/// — the caller is a background refresh or a keystroke — so a failure is
/// logged rather than raised, and never panics the thread that made it.
fn commit(txn: WriteTxn<'_>, what: &str) -> bool {
    match txn.commit() {
        Ok(()) => true,
        Err(e) => {
            crate::log::error(format!("cache write failed ({}): {}", what, e));
            crate::log::news(format!("cache write failed ({}): {}", what, e));
            false
        }
    }
}

impl Clone for Cache {
    fn clone(&self) -> Self {
        Cache {
            store: Arc::clone(&self.store),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::tempdir;

    const FEED_URL: &str = "https://example.com/feed";
    const FEED_NAME: &str = "Example Feed";

    #[test]
    fn refresh_batch_inserts_articles_and_summarizes_feed() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        let summaries = cache.apply_refresh_batch(&[FeedRefresh {
            url: FEED_URL.to_string(),
            name: FEED_NAME.to_string(),
            title: FEED_NAME.to_string(),
            fetched_at: "2026-04-30 10:00:00".to_string(),
            articles: vec![article("a1", false)],
        }]);

        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].new_articles, 1);
        assert_eq!(summaries[0].total, 1);
        assert_eq!(summaries[0].unread, 1);
        assert!(cache.get_article("a1").is_some());
        assert_eq!(cache.get_feed_index(FEED_URL).unwrap(), vec!["a1"]);
    }

    #[test]
    fn refresh_batch_preserves_existing_read_state() {
        let dir = tempdir().expect("tempdir");
        let cache = Cache::open_at(dir.path().join("test.tdkv")).expect("cache");
        cache.put_articles(&[article("a1", true)]);
        cache.put_feed_index(FEED_URL, &["a1".to_string()]);

        let summaries = cache.apply_refresh_batch(&[FeedRefresh {
            url: FEED_URL.to_string(),
            name: FEED_NAME.to_string(),
            title: FEED_NAME.to_string(),
            fetched_at: "2026-04-30 10:00:00".to_string(),
            articles: vec![article("a1", false)],
        }]);

        assert_eq!(summaries[0].new_articles, 0);
        assert_eq!(summaries[0].unread, 0);
        assert!(cache.get_article("a1").unwrap().read);
    }

    /// The three tables move together: a batch is one transaction, and what
    /// a second process opens is either all of it or none.
    #[test]
    fn a_batch_survives_being_closed_and_opened_again() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("test.tdkv");
        {
            let cache = Cache::open_at(path.clone()).expect("cache");
            cache.apply_refresh_batch(&[FeedRefresh {
                url: FEED_URL.to_string(),
                name: FEED_NAME.to_string(),
                title: FEED_NAME.to_string(),
                fetched_at: "2026-04-30 10:00:00".to_string(),
                articles: vec![article("a1", false), article("a2", true)],
            }]);
        }
        let cache = Cache::open_at(path).expect("reopen");
        assert_eq!(cache.list_feed_urls(), vec![FEED_URL.to_string()]);
        assert_eq!(
            cache.get_feed_index(FEED_URL).unwrap(),
            vec!["a2".to_string(), "a1".to_string()]
        );
        assert!(cache.get_article("a2").unwrap().read);
        assert_eq!(
            cache.get_feed_meta(FEED_URL).unwrap().last_fetched,
            "2026-04-30 10:00:00"
        );
    }

    /// A file that is not this store at all — the old redb cache is one —
    /// is thrown away and replaced rather than reported. The cache is
    /// derived data; there is nothing a person could do with the news.
    /// `open_store` rather than `open_at`, because `open_at` logs, and the
    /// first test in a process to log fixes where every later one writes.
    #[test]
    fn an_unreadable_cache_is_replaced_with_an_empty_one() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("test.tdkv");
        std::fs::write(&path, b"redb\x00not this format at all").expect("write");
        let (store, replaced) = open_store(&path).expect("open");
        assert!(replaced.is_some(), "the unreadable file was read");
        let cache = Cache {
            store: Arc::new(store),
        };
        assert!(cache.list_feed_urls().is_empty());
        assert!(path.is_file());
        cache.put_articles(&[article("a1", false)]);
        assert!(cache.get_article("a1").is_some());

        // A store this one wrote opens without being replaced.
        drop(cache);
        let (_, replaced) = open_store(&path).expect("reopen");
        assert!(replaced.is_none());
    }

    /// The old cache is deleted rather than left behind holding a copy of
    /// every article ever fetched. The default directory is the process
    /// environment's, so what is exercised here is the part that takes a
    /// directory.
    #[test]
    fn the_redb_cache_is_removed_from_the_cache_directory() {
        let dir = tempdir().expect("tempdir");
        let legacy = dir.path().join(LEGACY_CACHE_FILE);
        std::fs::write(&legacy, b"old redb bytes").expect("write");
        let kept = dir.path().join(CACHE_FILE);
        std::fs::write(&kept, b"").expect("write");
        remove_legacy_cache_in(dir.path());
        assert!(!legacy.exists(), "the redb cache was left behind");
        assert!(kept.is_file(), "the new cache was removed");
        // Removing what is not there is not a failure.
        remove_legacy_cache_in(dir.path());
    }

    fn article(hash: &str, read: bool) -> Article {
        Article {
            hash: hash.to_string(),
            title: format!("Article {}", hash),
            link: format!("https://example.com/{}", hash),
            description: "desc".to_string(),
            content: "content".to_string(),
            published: Some("2026-04-30 09:00:00".to_string()),
            feed_name: FEED_NAME.to_string(),
            read,
        }
    }
}
