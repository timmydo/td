use std::path::PathBuf;
use std::sync::OnceLock;

use crate::civil::{self, Zone};
use crate::json::{Json, ToJson};
use crate::xml::{Event, Reader};

/// A normalized article parsed from an RSS or Atom feed.
#[derive(Debug, Clone)]
pub struct Article {
    pub hash: String,
    pub title: String,
    pub link: String,
    pub description: String,
    pub content: String,
    pub published: Option<String>,
    pub feed_name: String,
    pub read: bool,
}

/// Metadata about a feed after fetching.
#[derive(Debug, Clone)]
pub struct FeedMeta {
    pub url: String,
    pub title: String,
    pub last_fetched: String,
}

/// A required string member.
fn string_field(value: &Json, key: &str) -> Option<String> {
    Some(value.get(key)?.as_str()?.to_string())
}

/// A member that may be absent or null, and is otherwise a string.
fn optional_string_field(value: &Json, key: &str) -> Option<Option<String>> {
    match value.get(key) {
        None | Some(Json::Null) => Some(None),
        Some(found) => Some(Some(found.as_str()?.to_string())),
    }
}

impl ToJson for Article {
    /// Members in declaration order, which is what `to_string` writes.
    fn to_json(&self) -> Json {
        crate::json!({
            "hash": self.hash,
            "title": self.title,
            "link": self.link,
            "description": self.description,
            "content": self.content,
            "published": self.published,
            "feed_name": self.feed_name,
            "read": self.read,
        })
    }
}

impl Article {
    /// The reader for [`Article::to_json`]. `None` for a member that is
    /// missing or of the wrong type; unknown members are ignored, as serde
    /// ignored them.
    pub fn from_json(value: &Json) -> Option<Article> {
        Some(Article {
            hash: string_field(value, "hash")?,
            title: string_field(value, "title")?,
            link: string_field(value, "link")?,
            description: string_field(value, "description")?,
            content: string_field(value, "content")?,
            published: optional_string_field(value, "published")?,
            feed_name: string_field(value, "feed_name")?,
            read: value.get("read")?.as_bool()?,
        })
    }
}

impl ToJson for FeedMeta {
    fn to_json(&self) -> Json {
        crate::json!({
            "url": self.url,
            "title": self.title,
            "last_fetched": self.last_fetched,
        })
    }
}

impl FeedMeta {
    /// The reader for [`FeedMeta::to_json`].
    pub fn from_json(value: &Json) -> Option<FeedMeta> {
        Some(FeedMeta {
            url: string_field(value, "url")?,
            title: string_field(value, "title")?,
            last_fetched: string_field(value, "last_fetched")?,
        })
    }
}

/// Where the fetch service's socket belongs, as text: the path when the
/// runtime directory is known, and the variable's name when it is not,
/// since an unset `XDG_RUNTIME_DIR` is then the thing that is missing.
fn fetch_socket_location() -> String {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime)
            .join("td-fetch")
            .join("socket")
            .display()
            .to_string(),
        None => "$XDG_RUNTIME_DIR/td-fetch/socket".to_string(),
    }
}

/// td-news holds no TLS, no trust store and no resolver: the network is
/// `td-fetchd`'s and this program holds a unix socket (td's
/// APPLICATIONS.md §W.8). Off a td session there is no such service, and
/// that is a missing component rather than a network failure, so it is
/// named as one — what is absent, where it belongs, and what to do about
/// it on a host (§X).
fn no_fetch_service() -> String {
    format!(
        "no td-fetch socket at {}: td-news fetches through td's fetch service; \
         on a host, serve one there",
        fetch_socket_location()
    )
}

/// Fetch and parse an RSS/Atom feed from the given URL.
/// Returns the feed title and a list of articles.
pub fn fetch_feed(url: &str, feed_name: &str) -> Result<(String, Vec<Article>), String> {
    if !crate::td_fetch::available() {
        return Err(format!("fetch {}: {}", url, no_fetch_service()));
    }
    // Inside a td jail that carries `sockets=fetch`: the fetch service
    // holds the network, this program holds a socket (td's
    // APPLICATIONS.md §W.8). The service names its refusals, and a
    // status is an answer to be shown, not a transport error.
    let response =
        crate::td_fetch::get(url, &[], None, None).map_err(|e| format!("fetch {}: {}", url, e))?;
    if !(200..300).contains(&response.status) {
        return Err(format!("fetch {}: HTTP {}", url, response.status));
    }
    let body =
        String::from_utf8(response.body).map_err(|e| format!("read body from {}: {}", url, e))?;

    parse_feed(&body, feed_name)
}

/// Parse RSS/Atom XML into articles.
fn parse_feed(xml: &str, feed_name: &str) -> Result<(String, Vec<Article>), String> {
    // Detect feed type by looking for <feed (Atom) vs <rss or <channel (RSS)
    let trimmed = xml.trim_start();
    if trimmed.contains("<feed") && !trimmed.contains("<rss") {
        parse_atom(xml, feed_name)
    } else {
        parse_rss(xml, feed_name)
    }
}

/// Parse an RSS 2.0 feed.
fn parse_rss(xml: &str, feed_name: &str) -> Result<(String, Vec<Article>), String> {
    let mut reader = Reader::new(xml);

    let mut feed_title = String::new();
    let mut articles = Vec::new();

    // State tracking
    let mut in_channel = false;
    let mut in_item = false;
    let mut current_tag = String::new();

    // Item fields
    let mut item_title = String::new();
    let mut item_link = String::new();
    let mut item_description = String::new();
    let mut item_content = String::new();
    let mut item_pub_date = String::new();

    loop {
        match reader.next() {
            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => match tag.name.as_str() {
                "channel" => in_channel = true,
                "item" => {
                    in_item = true;
                    item_title.clear();
                    item_link.clear();
                    item_description.clear();
                    item_content.clear();
                    item_pub_date.clear();
                }
                _ => current_tag = tag.name,
            },
            Ok(Event::End(name)) => {
                if name == "item" {
                    let link = item_link.trim().to_string();
                    let title = item_title.trim().to_string();
                    let hash = article_hash(&title, &link);
                    articles.push(Article {
                        hash,
                        title,
                        link,
                        description: item_description.trim().to_string(),
                        content: item_content.trim().to_string(),
                        published: normalize_optional_datetime(&item_pub_date),
                        feed_name: feed_name.to_string(),
                        read: false,
                    });
                    in_item = false;
                } else if name == "channel" {
                    in_channel = false;
                }
                current_tag.clear();
            }
            Ok(Event::Text(text)) | Ok(Event::CData(text)) => {
                rss_apply_text(
                    &text,
                    in_item,
                    in_channel,
                    &current_tag,
                    &mut item_title,
                    &mut item_link,
                    &mut item_description,
                    &mut item_content,
                    &mut item_pub_date,
                    &mut feed_title,
                );
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
        }
    }

    if feed_title.is_empty() {
        feed_title = feed_name.to_string();
    }

    Ok((feed_title, articles))
}

#[allow(clippy::too_many_arguments)]
fn rss_apply_text(
    text: &str,
    in_item: bool,
    in_channel: bool,
    current_tag: &str,
    item_title: &mut String,
    item_link: &mut String,
    item_description: &mut String,
    item_content: &mut String,
    item_pub_date: &mut String,
    feed_title: &mut String,
) {
    if in_item {
        match current_tag {
            "title" => item_title.push_str(text),
            "link" => item_link.push_str(text),
            "description" => item_description.push_str(text),
            "content:encoded" | "content" => item_content.push_str(text),
            "pubDate" | "dc:date" => item_pub_date.push_str(text),
            _ => {}
        }
    } else if in_channel && current_tag == "title" {
        feed_title.push_str(text);
    }
}

/// Parse an Atom feed.
fn parse_atom(xml: &str, feed_name: &str) -> Result<(String, Vec<Article>), String> {
    let mut reader = Reader::new(xml);

    let mut feed_title = String::new();
    let mut articles = Vec::new();

    let mut in_feed = false;
    let mut in_entry = false;
    let mut current_tag = String::new();

    let mut entry_title = String::new();
    let mut entry_link = String::new();
    let mut entry_summary = String::new();
    let mut entry_content = String::new();
    let mut entry_published = String::new();

    loop {
        match reader.next() {
            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => {
                match tag.name.as_str() {
                    "feed" => in_feed = true,
                    "entry" => {
                        in_entry = true;
                        entry_title.clear();
                        entry_link.clear();
                        entry_summary.clear();
                        entry_content.clear();
                        entry_published.clear();
                    }
                    "link" => {
                        // Atom links are attributes: <link href="..." rel="alternate" />
                        let href = tag.attr("href").unwrap_or_default();
                        let rel = tag.attr("rel").unwrap_or_default();
                        // Use alternate link, or any link if no rel specified
                        if in_entry
                            && (rel.is_empty() || rel == "alternate")
                            && entry_link.is_empty()
                        {
                            entry_link = href.to_string();
                        }
                    }
                    _ => current_tag = tag.name,
                }
            }
            Ok(Event::End(name)) => {
                if name == "entry" {
                    let link = entry_link.trim().to_string();
                    let title = entry_title.trim().to_string();
                    let hash = article_hash(&title, &link);
                    articles.push(Article {
                        hash,
                        title,
                        link,
                        description: entry_summary.trim().to_string(),
                        content: entry_content.trim().to_string(),
                        published: normalize_optional_datetime(&entry_published),
                        feed_name: feed_name.to_string(),
                        read: false,
                    });
                    in_entry = false;
                } else if name == "feed" {
                    in_feed = false;
                }
                current_tag.clear();
            }
            Ok(Event::Text(text)) | Ok(Event::CData(text)) => {
                atom_apply_text(
                    &text,
                    in_entry,
                    in_feed,
                    &current_tag,
                    &mut entry_title,
                    &mut entry_summary,
                    &mut entry_content,
                    &mut entry_published,
                    &mut feed_title,
                );
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("XML parse error: {}", e)),
        }
    }

    if feed_title.is_empty() {
        feed_title = feed_name.to_string();
    }

    Ok((feed_title, articles))
}

#[allow(clippy::too_many_arguments)]
fn atom_apply_text(
    text: &str,
    in_entry: bool,
    in_feed: bool,
    current_tag: &str,
    entry_title: &mut String,
    entry_summary: &mut String,
    entry_content: &mut String,
    entry_published: &mut String,
    feed_title: &mut String,
) {
    if in_entry {
        match current_tag {
            "title" => entry_title.push_str(text),
            "summary" => entry_summary.push_str(text),
            "content" => entry_content.push_str(text),
            "published" | "updated" if entry_published.is_empty() => entry_published.push_str(text),
            _ => {}
        }
    } else if in_feed && current_tag == "title" {
        feed_title.push_str(text);
    }
}

/// Compute a deduplication hash for an article (matching feed2maildir's approach).
pub fn article_hash(title: &str, link: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    title.hash(&mut hasher);
    link.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The machine's zone, read once. `/etc/localtime` does not change under a
/// running program, and a feed of a thousand items would otherwise read and
/// parse it a thousand times.
pub fn local_zone() -> &'static Zone {
    static ZONE: OnceLock<Zone> = OnceLock::new();
    ZONE.get_or_init(Zone::local)
}

/// An instant as local wall-clock text: the `%Y-%m-%d %H:%M:%S` the cache
/// stores and every view shows.
pub fn format_local(unix: i64) -> String {
    let (wall, _) = local_zone().to_local(unix);
    civil::format_ymd_hms(&wall)
}

pub fn now_local_datetime_string() -> String {
    format_local(civil::now_unix())
}

pub fn normalize_datetime_to_local(input: &str) -> Option<String> {
    parse_datetime_to_unix(input).map(format_local)
}

pub fn datetime_sort_key(input: Option<&str>) -> Option<i64> {
    parse_datetime_to_unix(input?)
}

fn normalize_optional_datetime(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        normalize_datetime_to_local(trimmed)
    }
}

/// A feed's date, in any of the four shapes feeds write, as an instant.
/// The two RFCs carry their own offset; the three bare civil forms are read
/// in the machine's zone, taking the one instant that exists, or the
/// earlier of two where a clock went back.
fn parse_datetime_to_unix(input: &str) -> Option<i64> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }

    if let Some(unix) = civil::parse_rfc3339(s) {
        return Some(unix);
    }
    if let Some(unix) = civil::parse_rfc2822(s) {
        return Some(unix);
    }

    let zone = local_zone();
    for wall in [
        civil::parse_ymd_hms(s),
        civil::parse_ymd_hm(s),
        civil::parse_ymd(s),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(unix) = zone
            .from_local_single(&wall)
            .or_else(|| zone.from_local_earliest(&wall))
        {
            return Some(unix);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off a td session there is no fetch service, and td-news cannot reach a
    /// feed at all. That is a missing component, so the message names it
    /// and says where it belongs, rather than reading as a network fault.
    #[test]
    fn without_the_fetch_service_a_feed_says_what_is_missing() {
        if crate::td_fetch::available() {
            // A td session, or a host someone has served a socket on:
            // there is nothing to be missing.
            return;
        }
        let err = fetch_feed("https://example.com/rss", "Example").unwrap_err();
        assert!(
            err.starts_with("fetch https://example.com/rss: no td-fetch socket at "),
            "{err}"
        );
        assert!(
            err.ends_with("/td-fetch/socket: td-news fetches through td's fetch service; on a host, serve one there"),
            "{err}"
        );
        assert!(fetch_socket_location().ends_with("/td-fetch/socket"));
    }

    /// The four shapes a feed writes a date in, and what the cache stores.
    /// The two RFC forms carry their own offset, so their instant is exact
    /// whatever zone this runs in; the three bare forms are the machine's
    /// wall clock, so what is pinned is that they read back as themselves.
    #[test]
    fn a_feed_date_is_read_in_every_shape_a_feed_writes_it() {
        let key = |s: &str| datetime_sort_key(Some(s));

        // RFC 3339, with and without an offset, seconds and fractions.
        assert_eq!(key("2024-01-01T00:00:00Z"), Some(1_704_067_200));
        assert_eq!(key("2024-01-01T01:00:00+01:00"), Some(1_704_067_200));
        assert_eq!(key("2023-12-31T19:00:00-05:00"), Some(1_704_067_200));
        assert_eq!(key("2024-01-01T00:00:00.750Z"), Some(1_704_067_200));
        // RFC 2822, as an RSS `pubDate` writes it.
        assert_eq!(key("Mon, 01 Jan 2024 00:00:00 GMT"), Some(1_704_067_200));
        assert_eq!(key("Mon, 1 Jan 2024 00:00:00 +0000"), Some(1_704_067_200));
        assert_eq!(key("01 Jan 2024 00:00:00 GMT"), Some(1_704_067_200));

        // The three bare civil forms, in the machine's zone. An hour a
        // clock skipped has no instant; one it repeated has two, and the
        // earlier is taken, so a round trip through the stored text is
        // stable whichever this is.
        for text in ["2024-06-02 03:04:05", "2024-06-02 03:04", "2024-06-02"] {
            let unix = key(text).unwrap_or_else(|| panic!("{text} did not parse"));
            let stored = format_local(unix);
            assert_eq!(normalize_datetime_to_local(text).as_deref(), Some(&*stored));
            assert_eq!(key(&stored).map(format_local).as_deref(), Some(&*stored));
            assert_eq!(stored.len(), "2024-06-02 03:04:05".len());
        }

        // Later sorts later, which is what the article lists order on.
        assert!(key("2024-01-02T00:00:00Z") > key("2024-01-01T00:00:00Z"));

        // Nothing, whitespace and prose are not dates.
        assert_eq!(datetime_sort_key(None), None);
        for text in ["", "   ", "yesterday", "2024-13-01", "2024-02-30"] {
            assert_eq!(key(text), None, "{text:?}");
            assert_eq!(normalize_datetime_to_local(text), None, "{text:?}");
        }

        // The clock reads as the same shape everything else is stored in.
        let now = now_local_datetime_string();
        assert_eq!(key(&now).map(format_local).as_deref(), Some(&*now));
    }

    #[test]
    fn parse_rss_feed() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Test Feed</title>
    <link>https://example.com</link>
    <item>
      <title>First Post</title>
      <link>https://example.com/1</link>
      <description>Hello world</description>
      <pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>
    </item>
    <item>
      <title>Second Post</title>
      <link>https://example.com/2</link>
      <description>Another post</description>
    </item>
  </channel>
</rss>"#;
        let (title, articles) = parse_feed(xml, "Test").unwrap();
        assert_eq!(title, "Test Feed");
        assert_eq!(articles.len(), 2);
        assert_eq!(articles[0].title, "First Post");
        assert_eq!(articles[0].link, "https://example.com/1");
        assert_eq!(articles[0].description, "Hello world");
        assert!(articles[0].published.is_some());
        assert_eq!(articles[1].title, "Second Post");
        assert!(articles[1].published.is_none());
        assert!(!articles[0].read);
        assert_eq!(articles[0].feed_name, "Test");
    }

    #[test]
    fn parse_atom_feed() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Atom Feed</title>
  <entry>
    <title>Atom Entry</title>
    <link href="https://example.com/atom/1" rel="alternate" />
    <summary>An atom summary</summary>
    <content>Full content here</content>
    <published>2024-01-01T00:00:00Z</published>
  </entry>
</feed>"#;
        let (title, articles) = parse_feed(xml, "AtomTest").unwrap();
        assert_eq!(title, "Atom Feed");
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Atom Entry");
        assert_eq!(articles[0].link, "https://example.com/atom/1");
        assert_eq!(articles[0].description, "An atom summary");
        assert_eq!(articles[0].content, "Full content here");
        assert!(articles[0].published.is_some());
    }

    #[test]
    fn parse_rss_with_cdata() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>CDATA Feed</title>
    <item>
      <title><![CDATA[Post with <special> chars]]></title>
      <link>https://example.com/cdata</link>
      <description><![CDATA[<p>HTML content</p>]]></description>
    </item>
  </channel>
</rss>"#;
        let (_, articles) = parse_feed(xml, "Test").unwrap();
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "Post with <special> chars");
        assert_eq!(articles[0].description, "<p>HTML content</p>");
    }

    /// The shape the cache and the CLI now write, and read back.
    #[test]
    fn an_article_and_its_feed_survive_a_json_round_trip() {
        let article = Article {
            hash: "h".to_string(),
            title: "T".to_string(),
            link: "https://example.com/1".to_string(),
            description: "d".to_string(),
            content: "c".to_string(),
            published: Some("2026-01-02 03:04:05".to_string()),
            feed_name: "F".to_string(),
            read: true,
        };
        let text = article.to_json().to_string();
        assert_eq!(
            text,
            concat!(
                r#"{"hash":"h","title":"T","link":"https://example.com/1","#,
                r#""description":"d","content":"c","#,
                r#""published":"2026-01-02 03:04:05","feed_name":"F","read":true}"#
            )
        );
        let back = Article::from_json(&crate::json::parse(&text).unwrap()).unwrap();
        assert_eq!(back.to_json().to_string(), text);

        // An absent or null `published` reads as none, as serde's
        // `Option<String>` did.
        let mut without = crate::json::parse(&text).unwrap();
        assert!(without.remove("published").is_some());
        assert!(Article::from_json(&without).unwrap().published.is_none());
        let null = crate::json::parse(&text.replace(r#""2026-01-02 03:04:05""#, "null")).unwrap();
        assert!(Article::from_json(&null).unwrap().published.is_none());

        // A missing member, or one of the wrong type, is not an article.
        let mut short = crate::json::parse(&text).unwrap();
        assert!(short.remove("title").is_some());
        assert!(Article::from_json(&short).is_none());
        let wrong = crate::json::parse(&text.replace(r#""read":true"#, r#""read":"yes""#)).unwrap();
        assert!(Article::from_json(&wrong).is_none());

        let meta = FeedMeta {
            url: "https://example.com/feed".to_string(),
            title: "F".to_string(),
            last_fetched: "2026-01-02 03:04:05".to_string(),
        };
        let text = meta.to_json().to_string();
        assert_eq!(
            text,
            r#"{"url":"https://example.com/feed","title":"F","last_fetched":"2026-01-02 03:04:05"}"#
        );
        let back = FeedMeta::from_json(&crate::json::parse(&text).unwrap()).unwrap();
        assert_eq!(back.to_json().to_string(), text);
    }

    #[test]
    fn article_hash_deterministic() {
        let h1 = article_hash("title", "link");
        let h2 = article_hash("title", "link");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
    }

    #[test]
    fn article_hash_differs() {
        let h1 = article_hash("title1", "link");
        let h2 = article_hash("title2", "link");
        assert_ne!(h1, h2);
    }
}
