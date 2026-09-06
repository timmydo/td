//! The CLI protocol end to end: a seeded cache, the binary, and the NDJSON
//! frames it writes back. The cache is seeded and the replies are read
//! with the same `json` and `kv` modules the program uses, included here
//! rather than linked, because `td-news` is a binary crate.

#[path = "../src/json.rs"]
#[allow(dead_code)]
mod json;
#[path = "../src/kv.rs"]
#[allow(dead_code)]
mod kv;
#[path = "../src/testing.rs"]
#[allow(dead_code)]
mod testing;

use json::Json;
use kv::{Key, Store};
use std::io::Write;
use std::process::{Command, Stdio};
use testing::tempdir;

const ARTICLES: &str = "articles";
const FEEDS: &str = "feeds";
const FEED_INDEX: &str = "feed_index";

#[test]
fn help_cli_includes_command_docs() {
    let output = Command::new(env!("CARGO_BIN_EXE_td-news"))
        .arg("--help-cli")
        .output()
        .expect("run td-news --help-cli");
    assert!(output.status.success());

    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(stderr.contains("list_folders"));
    assert!(stderr.contains("list_articles"));
    assert!(stderr.contains("get_article"));
}

#[test]
fn cli_mode_reads_json_commands_and_returns_json_lines() {
    let dir = tempdir().expect("tempdir");
    let cache_path = dir.path().join("test.tdkv");
    seed_cache(&cache_path);

    let mut child = Command::new(env!("CARGO_BIN_EXE_td-news"))
        .args(["--cli", "--cache"])
        .arg(&cache_path)
        // Its logs go with the cache, not to whoever runs the tests.
        .env("XDG_CACHE_HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn td-news --cli");

    let input = concat!(
        "{\"cmd\":\"list_folders\"}\n",
        "{\"cmd\":\"list_articles\",\"folder\":\"https://example.com/feed\"}\n",
        "{\"cmd\":\"get_article\",\"hash\":\"a1\"}\n",
        "{\"cmd\":\"mark_read\",\"hash\":\"a1\",\"read\":true}\n",
        "{\"cmd\":\"get_article\",\"hash\":\"a1\"}\n",
        "{\"cmd\":\"quit\"}\n"
    );
    let stdin = child.stdin.as_mut().expect("stdin");
    stdin.write_all(input.as_bytes()).expect("write stdin");

    let output = child.wait_with_output().expect("wait output");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 6);

    let responses: Vec<Json> = lines
        .iter()
        .map(|line| json::parse(line).expect("valid json line"))
        .collect();

    assert_eq!(responses[0]["ok"], Json::Bool(true));
    let folders = responses[0]["folders"].as_arr().expect("folders array");
    assert!(folders
        .iter()
        .any(|f| f["id"] == Json::Str("https://example.com/feed".to_string())));

    assert_eq!(responses[1]["ok"], Json::Bool(true));
    assert_eq!(responses[1]["total"].as_u64(), Some(2));

    assert_eq!(responses[2]["ok"], Json::Bool(true));
    assert_eq!(
        responses[2]["article"]["title"],
        Json::Str("First".to_string())
    );
    assert_eq!(responses[2]["article"]["read"], Json::Bool(false));

    assert_eq!(responses[3]["ok"], Json::Bool(true));
    assert_eq!(responses[3]["read"], Json::Bool(true));

    assert_eq!(responses[4]["ok"], Json::Bool(true));
    assert_eq!(responses[4]["article"]["read"], Json::Bool(true));

    assert_eq!(responses[5]["ok"], Json::Bool(true));
    assert_eq!(responses[5]["quit"], Json::Bool(true));
}

fn seed_cache(path: &std::path::Path) {
    // Scoped: the store takes an exclusive lock on its log, so it must be
    // closed before the binary under test opens the same file.
    let store = Store::open(path).expect("create store");
    let mut txn = store.write();
    let a1 = article(
        "a1",
        "First",
        "desc1",
        "content1",
        "2025-01-02 03:04:05",
        false,
    );
    let a2 = article(
        "a2",
        "Second",
        "desc2",
        "content2",
        "2025-01-01 03:04:05",
        true,
    );
    txn.insert(ARTICLES, &Key::from_str("a1"), &a1.to_vec());
    txn.insert(ARTICLES, &Key::from_str("a2"), &a2.to_vec());

    let feed_meta = crate::json!({
        "url": "https://example.com/feed",
        "title": "Example Feed",
        "last_fetched": "2025-01-03 10:00:00",
    });
    txn.insert(
        FEEDS,
        &Key::from_str("https://example.com/feed"),
        &feed_meta.to_vec(),
    );

    let hashes = crate::json!(["a1", "a2"]);
    txn.insert(
        FEED_INDEX,
        &Key::from_str("https://example.com/feed"),
        &hashes.to_vec(),
    );

    txn.commit().expect("commit");
}

fn article(
    hash: &str,
    title: &str,
    description: &str,
    content: &str,
    published: &str,
    read: bool,
) -> Json {
    crate::json!({
        "hash": hash,
        "title": title,
        "link": format!("https://example.com/{}", hash.trim_start_matches('a')),
        "description": description,
        "content": content,
        "published": published,
        "feed_name": "Example Feed",
        "read": read,
    })
}
