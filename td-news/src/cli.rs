use std::cmp::Ordering;
use std::io::{self, BufRead, Write};

use crate::cache::Cache;
use crate::feed::{datetime_sort_key, Article};
use crate::json::{self, Json};

const MAX_LIMIT: usize = 1000;

/// The `cmd` values, in the order the help text lists them; also the
/// `expected one of` list an unknown one is reported against.
const COMMANDS: &[&str] = &[
    "list_folders",
    "list_articles",
    "get_article",
    "mark_read",
    "mark_folder_read",
    "search_articles",
    "help",
    "quit",
];

fn default_search_folder() -> String {
    "all".to_string()
}

#[derive(Debug)]
enum Command {
    ListFolders,
    ListArticles {
        folder: String,
        offset: usize,
        limit: Option<usize>,
    },
    GetArticle {
        hash: String,
    },
    MarkRead {
        hash: String,
        read: bool,
    },
    MarkFolderRead {
        folder: String,
    },
    SearchArticles {
        query: String,
        folder: String,
        offset: usize,
        limit: Option<usize>,
    },
    Help,
    Quit,
}

/// serde's `missing field` text, which the protocol's error line carried.
fn missing_field(field: &str) -> String {
    format!("missing field `{}`", field)
}

/// serde's `invalid type` text, naming the member rather than a path.
fn invalid_type(field: &str, expected: &str) -> String {
    format!("invalid type for `{}`: expected {}", field, expected)
}

fn require_string(value: &Json, field: &str) -> Result<String, String> {
    match value.get(field) {
        None => Err(missing_field(field)),
        Some(found) => found
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| invalid_type(field, "a string")),
    }
}

fn require_bool(value: &Json, field: &str) -> Result<bool, String> {
    match value.get(field) {
        None => Err(missing_field(field)),
        Some(found) => found
            .as_bool()
            .ok_or_else(|| invalid_type(field, "a boolean")),
    }
}

/// An absent or null member takes the default, as `#[serde(default)]` did.
fn optional_usize(value: &Json, field: &str) -> Result<Option<usize>, String> {
    match value.get(field) {
        None | Some(Json::Null) => Ok(None),
        Some(found) => found
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| invalid_type(field, "a non-negative integer")),
    }
}

impl Command {
    /// The internally tagged reader `#[serde(tag = "cmd")]` generated:
    /// `cmd` selects the variant and the remaining members are its fields.
    /// Members the variant does not name are ignored, as serde ignored them.
    fn from_json(value: &Json) -> Result<Command, String> {
        if value.as_obj().is_none() {
            return Err("invalid type: expected a JSON object".to_string());
        }
        let cmd = require_string(value, "cmd")?;
        match cmd.as_str() {
            "list_folders" => Ok(Command::ListFolders),
            "list_articles" => Ok(Command::ListArticles {
                folder: require_string(value, "folder")?,
                offset: optional_usize(value, "offset")?.unwrap_or(0),
                limit: optional_usize(value, "limit")?,
            }),
            "get_article" => Ok(Command::GetArticle {
                hash: require_string(value, "hash")?,
            }),
            "mark_read" => Ok(Command::MarkRead {
                hash: require_string(value, "hash")?,
                read: require_bool(value, "read")?,
            }),
            "mark_folder_read" => Ok(Command::MarkFolderRead {
                folder: require_string(value, "folder")?,
            }),
            "search_articles" => Ok(Command::SearchArticles {
                query: require_string(value, "query")?,
                folder: match value.get("folder") {
                    None | Some(Json::Null) => default_search_folder(),
                    Some(found) => found
                        .as_str()
                        .map(str::to_string)
                        .ok_or_else(|| invalid_type("folder", "a string"))?,
                },
                offset: optional_usize(value, "offset")?.unwrap_or(0),
                limit: optional_usize(value, "limit")?,
            }),
            "help" => Ok(Command::Help),
            "quit" => Ok(Command::Quit),
            other => Err(format!(
                "unknown variant `{}`, expected one of {}",
                other,
                COMMANDS
                    .iter()
                    .map(|name| format!("`{}`", name))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

pub fn run(cache: &Cache) -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(line) => line,
            Err(e) => return Err(format!("failed to read stdin: {}", e)),
        };

        if line.trim().is_empty() {
            continue;
        }

        let command = match json::parse(&line)
            .map_err(|e| e.to_string())
            .and_then(|value| Command::from_json(&value))
        {
            Ok(cmd) => cmd,
            Err(e) => {
                write_json_line(
                    &mut stdout,
                    &crate::json!({
                        "ok": false,
                        "error": format!("invalid command JSON: {}", e),
                    }),
                )?;
                continue;
            }
        };

        let response = handle_command(cache, command);
        write_json_line(&mut stdout, &response)?;

        if response.get("quit").and_then(Json::as_bool) == Some(true) {
            break;
        }
    }

    Ok(())
}

/// One NDJSON frame: the compact form, a newline, and a flush. Writing a
/// `Json` cannot fail, so the only errors here are the terminal's.
fn write_json_line(stdout: &mut dyn Write, value: &Json) -> Result<(), String> {
    writeln!(stdout, "{}", value).map_err(|e| format!("failed to write stdout: {}", e))?;
    stdout
        .flush()
        .map_err(|e| format!("failed to flush stdout: {}", e))
}

fn handle_command(cache: &Cache, command: Command) -> Json {
    match command {
        Command::ListFolders => {
            let mut folders = Vec::new();
            let all = sort_articles(cache.list_all_articles());
            let unread_count = all.iter().filter(|a| !a.read).count();

            folders.push(crate::json!({
                "id": "all",
                "name": "All",
                "total": all.len(),
                "unread": unread_count,
                "virtual": true,
            }));
            folders.push(crate::json!({
                "id": "unread",
                "name": "Unread",
                "total": unread_count,
                "unread": unread_count,
                "virtual": true,
            }));

            for url in cache.list_feed_urls() {
                let articles = cache.list_feed_articles(&url);
                let total = articles.len();
                let unread = articles.iter().filter(|a| !a.read).count();
                let meta = cache.get_feed_meta(&url);
                let name = meta
                    .as_ref()
                    .map(|m| m.title.clone())
                    .or_else(|| articles.first().map(|a| a.feed_name.clone()))
                    .unwrap_or_else(|| url.clone());
                folders.push(crate::json!({
                    "id": url,
                    "name": name,
                    "url": meta.as_ref().map(|m| m.url.clone()),
                    "total": total,
                    "unread": unread,
                    "last_fetched": meta.map(|m| m.last_fetched),
                    "virtual": false,
                }));
            }

            crate::json!({
                "ok": true,
                "folders": folders,
            })
        }
        Command::ListArticles {
            folder,
            offset,
            limit,
        } => {
            let limit = limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT);
            let articles = match folder_articles(cache, &folder) {
                Ok(articles) => sort_articles(articles),
                Err(error) => {
                    return crate::json!({
                        "ok": false,
                        "error": error,
                    })
                }
            };

            let total = articles.len();
            let items: Vec<_> = articles
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|a| {
                    crate::json!({
                        "hash": a.hash,
                        "title": a.title,
                        "link": a.link,
                        "published": a.published,
                        "feed_name": a.feed_name,
                        "read": a.read,
                    })
                })
                .collect();

            crate::json!({
                "ok": true,
                "folder": folder,
                "offset": offset,
                "limit": limit,
                "total": total,
                "articles": items,
            })
        }
        Command::GetArticle { hash } => match cache.get_article(&hash) {
            Some(article) => crate::json!({
                "ok": true,
                "article": article,
            }),
            None => crate::json!({
                "ok": false,
                "error": format!("article not found: {}", hash),
            }),
        },
        Command::MarkRead { hash, read } => {
            if cache.get_article(&hash).is_none() {
                crate::json!({
                    "ok": false,
                    "error": format!("article not found: {}", hash),
                })
            } else {
                cache.mark_read(&hash, read);
                crate::json!({
                    "ok": true,
                    "hash": hash,
                    "read": read,
                })
            }
        }
        Command::MarkFolderRead { folder } => {
            let hashes = match folder_articles(cache, &folder) {
                Ok(articles) => articles.into_iter().map(|a| a.hash).collect::<Vec<_>>(),
                Err(error) => {
                    return crate::json!({
                        "ok": false,
                        "error": error,
                    })
                }
            };
            for hash in &hashes {
                cache.mark_read(hash, true);
            }
            crate::json!({
                "ok": true,
                "folder": folder,
                "updated": hashes.len(),
            })
        }
        Command::SearchArticles {
            query,
            folder,
            offset,
            limit,
        } => {
            let limit = limit.unwrap_or(MAX_LIMIT).min(MAX_LIMIT);
            let articles = match folder_articles(cache, &folder) {
                Ok(articles) => sort_articles(articles),
                Err(error) => {
                    return crate::json!({
                        "ok": false,
                        "error": error,
                    })
                }
            };

            let query_lower = query.to_lowercase();
            let matched: Vec<_> = articles
                .into_iter()
                .filter(|a| {
                    let hay = format!(
                        "{} {} {}",
                        a.title.to_lowercase(),
                        a.description.to_lowercase(),
                        a.content.to_lowercase()
                    );
                    hay.contains(&query_lower)
                })
                .collect();

            let total = matched.len();
            let items: Vec<_> = matched
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|a| {
                    crate::json!({
                        "hash": a.hash,
                        "title": a.title,
                        "link": a.link,
                        "published": a.published,
                        "feed_name": a.feed_name,
                        "read": a.read,
                    })
                })
                .collect();

            crate::json!({
                "ok": true,
                "query": query,
                "folder": folder,
                "offset": offset,
                "limit": limit,
                "total": total,
                "articles": items,
            })
        }
        Command::Help => crate::json!({
            "ok": true,
            "help": help_text(),
        }),
        Command::Quit => crate::json!({
            "ok": true,
            "quit": true,
        }),
    }
}

fn folder_articles(cache: &Cache, folder: &str) -> Result<Vec<Article>, String> {
    match folder {
        "all" => Ok(cache.list_all_articles()),
        "unread" => Ok(cache.list_unread_articles()),
        url => {
            if !cache.list_feed_urls().iter().any(|f| f == url) {
                return Err(format!("unknown folder: {}", folder));
            }
            Ok(cache.list_feed_articles(url))
        }
    }
}

fn sort_articles(mut articles: Vec<Article>) -> Vec<Article> {
    articles.sort_by(|a, b| {
        let a_ts = datetime_sort_key(a.published.as_deref()).unwrap_or(i64::MIN);
        let b_ts = datetime_sort_key(b.published.as_deref()).unwrap_or(i64::MIN);
        match b_ts.cmp(&a_ts) {
            Ordering::Equal => a.title.cmp(&b.title),
            other => other,
        }
    });
    articles
}

pub fn help_text() -> String {
    [
        "CLI mode protocol (newline-delimited JSON):",
        "- Input: one JSON object per line on stdin",
        "- Output: one JSON object per line on stdout",
        "",
        "Commands:",
        "- {\"cmd\":\"list_folders\"}",
        "  Returns folders at root level. Includes virtual folders: all, unread.",
        "",
        "- {\"cmd\":\"list_articles\",\"folder\":\"all|unread|<feed_url>\",\"offset\":0,\"limit\":100}",
        "  Lists article summaries for a folder. limit is capped at 1000.",
        "",
        "- {\"cmd\":\"get_article\",\"hash\":\"<article_hash>\"}",
        "  Returns full article contents.",
        "",
        "- {\"cmd\":\"mark_read\",\"hash\":\"<article_hash>\",\"read\":true}",
        "  Sets an article read/unread flag in cache.",
        "",
        "- {\"cmd\":\"mark_folder_read\",\"folder\":\"all|unread|<feed_url>\"}",
        "  Marks all articles in a folder as read.",
        "",
        "- {\"cmd\":\"search_articles\",\"query\":\"<text>\",\"folder\":\"all\",\"offset\":0,\"limit\":100}",
        "  Case-insensitive substring search on title, description, and content.",
        "  folder defaults to \"all\". Returns matching articles with total match count.",
        "",
        "- {\"cmd\":\"help\"}",
        "  Returns this command documentation as a string.",
        "",
        "- {\"cmd\":\"quit\"}",
        "  Requests graceful shutdown of CLI mode.",
        "",
        "Response format:",
        "- Success: {\"ok\":true,...}",
        "- Error: {\"ok\":false,\"error\":\"...\"}",
    ]
    .join("\n")
}
