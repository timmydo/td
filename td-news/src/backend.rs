use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cache::{Cache, FeedRefresh, FeedRefreshSummary};
use crate::config::Config;
use crate::feed::{datetime_sort_key, now_local_datetime_string};
use crate::log;

pub enum BackendCommand {
    FetchAllFeeds,
    FetchFeed { url: String },
    MarkRead { hash: String, read: bool },
    MarkFeedRead { feed_url: String },
    Shutdown,
}

pub enum BackendResponse {
    RefreshCompleted { reports: Vec<FeedRefreshReport> },
    ArticleMutation { hash: String, read: bool },
    FeedMarkedRead { feed_url: String },
}

pub struct FeedRefreshReport {
    pub feed_url: String,
    pub feed_name: String,
    pub fetched_at: Option<String>,
    pub new_articles: usize,
    pub total: usize,
    pub unread: usize,
    pub error: Option<String>,
}

pub fn spawn(
    config: &Config,
    cache: Cache,
) -> (
    mpsc::Sender<BackendCommand>,
    mpsc::Receiver<BackendResponse>,
) {
    let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
    let (resp_tx, resp_rx) = mpsc::channel::<BackendResponse>();

    let feeds = config.feeds.clone();
    let sync_interval = Duration::from_secs(config.ui.sync_interval_secs);

    std::thread::spawn(move || {
        log::info("backend thread started");
        log::news("backend thread started");
        backend_loop(cmd_rx, resp_tx, &cache, &feeds, sync_interval);
        log::info("backend thread stopped");
        log::news("backend thread stopped");
    });

    (cmd_tx, resp_rx)
}

fn backend_loop(
    cmd_rx: mpsc::Receiver<BackendCommand>,
    resp_tx: mpsc::Sender<BackendResponse>,
    cache: &Cache,
    feeds: &[crate::config::FeedConfig],
    sync_interval: Duration,
) {
    // When each feed was last tried, whether or not the fetch succeeded,
    // on the monotonic clock, which a stepped wall clock cannot move. The
    // cache records successes only; without this a feed that never
    // succeeds is due again the moment its fetch fails, and the loop
    // spins through failures, logging each.
    let mut attempts: HashMap<String, Instant> = HashMap::new();
    loop {
        let (due, timeout) = schedule(cache, feeds, sync_interval, &attempts);
        match cmd_rx.recv_timeout(timeout) {
            Ok(cmd) => match cmd {
                BackendCommand::FetchAllFeeds => {
                    log::info("backend command: FetchAllFeeds");
                    log::news("manual refresh: all feeds");
                    let reports = refresh_feeds(feeds.iter(), cache, &mut attempts);
                    let _ = resp_tx.send(BackendResponse::RefreshCompleted { reports });
                }
                BackendCommand::FetchFeed { url } => {
                    log::info(format!("backend command: FetchFeed {}", url));
                    let reports = refresh_feeds(
                        feeds.iter().filter(|feed| feed.url == url),
                        cache,
                        &mut attempts,
                    );
                    let _ = resp_tx.send(BackendResponse::RefreshCompleted { reports });
                }
                BackendCommand::MarkRead { hash, read } => {
                    log::info(format!("backend command: MarkRead {} => {}", hash, read));
                    cache.mark_read(&hash, read);
                    let _ = resp_tx.send(BackendResponse::ArticleMutation { hash, read });
                }
                BackendCommand::MarkFeedRead { feed_url } => {
                    log::info(format!("backend command: MarkFeedRead {}", feed_url));
                    if let Some(hashes) = cache.get_feed_index(&feed_url) {
                        for h in &hashes {
                            cache.mark_read(h, true);
                        }
                    }
                    let _ = resp_tx.send(BackendResponse::FeedMarkedRead { feed_url });
                }
                BackendCommand::Shutdown => break,
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Periodic refresh: the feeds found due before a zero
                // wait, or, after a real one, those the wait brought due.
                let due = if due.is_empty() {
                    schedule(cache, feeds, sync_interval, &attempts).0
                } else {
                    due
                };
                if due.is_empty() {
                    continue;
                }
                log::info(format!("backend periodic refresh: {} due", due.len()));
                log::news(format!("scheduled refresh: {} due feed(s)", due.len()));
                let reports = refresh_feeds(
                    due.into_iter().filter_map(|idx| feeds.get(idx)),
                    cache,
                    &mut attempts,
                );
                let _ = resp_tx.send(BackendResponse::RefreshCompleted { reports });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                log::warn("backend command channel disconnected");
                break;
            }
        }
    }
}

/// How long ago a wall-clock timestamp was, or nothing for one after
/// `now`, which a clock stepped back leaves behind: that feed is tried
/// once, and its attempt, on the monotonic clock, is what the interval
/// counts from.
fn seconds_since(now: i64, timestamp: Option<i64>) -> Option<i64> {
    timestamp
        .filter(|&stamp| stamp <= now)
        .map(|stamp| now.saturating_sub(stamp))
}

/// How long ago an attempt was, in whole seconds.
fn seconds_elapsed(since: &Instant) -> i64 {
    i64::try_from(since.elapsed().as_secs()).unwrap_or(i64::MAX)
}

/// The shortest interval: a feed asked more often than twice a minute is
/// a burden on its server.
const MIN_INTERVAL_SECS: i64 = 30;

/// How long the loop waits for a command when nothing is scheduled: a
/// wake then finds nothing due and waits again.
const IDLE_WAIT: Duration = Duration::from_secs(60 * 60);

/// The interval in whole seconds, at least `MIN_INTERVAL_SECS`; nothing
/// for a zero interval, which turns the periodic refresh off and leaves
/// the manual one. Before this, zero was the loop without pause that
/// this scheduling exists to prevent.
fn interval_seconds(sync_interval: Duration) -> Option<i64> {
    if sync_interval.is_zero() {
        return None;
    }
    Some(
        i64::try_from(sync_interval.as_secs())
            .unwrap_or(i64::MAX)
            .max(MIN_INTERVAL_SECS),
    )
}

/// Seconds until a feed is due, given how long ago its activities were:
/// its last attempt when it has one, else its last fetch, and a feed
/// with neither is due now. The attempt is this process's own and the
/// later of the two; the fetch stamp is the cache's, and one a stepped-
/// back clock read as the future when the attempt was made must not, in
/// range again, start the interval over.
fn seconds_until_due(interval_secs: i64, elapsed: [Option<i64>; 2]) -> i64 {
    let [fetched, attempted] = elapsed;
    match attempted.or(fetched) {
        Some(since) => interval_secs.saturating_sub(since).max(0),
        None => 0,
    }
}

/// A feed's activities, as how long ago each was: its last successful
/// fetch, from the cache, and its last attempt.
fn elapsed_activities(
    cache: &Cache,
    feed: &crate::config::FeedConfig,
    now: i64,
    attempts: &HashMap<String, Instant>,
) -> [Option<i64>; 2] {
    let last_fetched = cache
        .get_feed_meta(&feed.url)
        .and_then(|meta| datetime_sort_key(Some(&meta.last_fetched)));
    [
        seconds_since(now, last_fetched),
        attempts.get(&feed.url).map(seconds_elapsed),
    ]
}

/// Which feeds are due now, and how long until one is: due are those
/// whose last attempt, or last fetch before any, is an interval old,
/// and the wait is the
/// shortest remaining interval, one pass over the feeds at one reading
/// of the clock. With the periodic refresh off, nothing is due and the
/// wait is the idle one.
fn schedule(
    cache: &Cache,
    feeds: &[crate::config::FeedConfig],
    sync_interval: Duration,
    attempts: &HashMap<String, Instant>,
) -> (Vec<usize>, Duration) {
    let Some(interval_secs) = interval_seconds(sync_interval) else {
        return (Vec::new(), IDLE_WAIT);
    };
    let now = crate::civil::now_unix();
    let mut due = Vec::new();
    let mut wait = interval_secs;
    for (idx, feed) in feeds.iter().enumerate() {
        let remaining = seconds_until_due(
            interval_secs,
            elapsed_activities(cache, feed, now, attempts),
        );
        if remaining == 0 {
            due.push(idx);
        }
        wait = wait.min(remaining);
    }
    (due, Duration::from_secs(u64::try_from(wait).unwrap_or(0)))
}

fn refresh_feeds<'a, I>(
    feeds: I,
    cache: &Cache,
    attempts: &mut HashMap<String, Instant>,
) -> Vec<FeedRefreshReport>
where
    I: IntoIterator<Item = &'a crate::config::FeedConfig>,
{
    let mut refreshes = Vec::new();
    let mut report_order = Vec::new();

    for feed in feeds {
        let outcome = fetch_one_feed(&feed.url, &feed.name);
        // After the fetch, so a slow one does not eat into the pause.
        attempts.insert(feed.url.clone(), Instant::now());
        match outcome {
            Ok(refresh) => {
                report_order.push(Ok(refresh.url.clone()));
                refreshes.push(refresh);
            }
            Err(error) => {
                report_order.push(Err(FeedRefreshReport {
                    feed_url: feed.url.clone(),
                    feed_name: feed.name.clone(),
                    fetched_at: None,
                    new_articles: 0,
                    total: 0,
                    unread: 0,
                    error: Some(error),
                }));
            }
        }
    }

    let mut summaries_by_url = cache
        .apply_refresh_batch(&refreshes)
        .into_iter()
        .map(|summary| (summary.feed_url.clone(), summary))
        .collect::<std::collections::HashMap<_, _>>();

    report_order
        .into_iter()
        .filter_map(|item| match item {
            Ok(url) => summaries_by_url.remove(&url).map(report_from_summary),
            Err(report) => Some(report),
        })
        .collect()
}

fn report_from_summary(summary: FeedRefreshSummary) -> FeedRefreshReport {
    log::info(format!(
        "fetch success: {} ({}) new={} total={} unread={}",
        summary.feed_name, summary.feed_url, summary.new_articles, summary.total, summary.unread
    ));
    log::news(format!(
        "refresh done: {} ({}) new={} total={} unread={}",
        summary.feed_name, summary.feed_url, summary.new_articles, summary.total, summary.unread
    ));

    FeedRefreshReport {
        feed_url: summary.feed_url,
        feed_name: summary.feed_name,
        fetched_at: Some(summary.fetched_at),
        new_articles: summary.new_articles,
        total: summary.total,
        unread: summary.unread,
        error: None,
    }
}

fn fetch_one_feed(url: &str, name: &str) -> Result<FeedRefresh, String> {
    log::info(format!("fetch start: {} ({})", name, url));
    log::news(format!("refresh start: {} ({})", name, url));
    match crate::feed::fetch_feed(url, name) {
        Ok((title, articles)) => Ok(FeedRefresh {
            url: url.to_string(),
            name: name.to_string(),
            title,
            fetched_at: now_local_datetime_string(),
            articles,
        }),
        Err(e) => {
            log::error(format!("fetch error: {} ({}) {}", name, url, e));
            log::news(format!("refresh error: {} ({}) {}", name, url, e));
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_attempt_starts_the_interval_else_the_last_fetch() {
        // Never fetched, never tried: due now.
        assert_eq!(seconds_until_due(300, [None, None]), 0);
        // Tried just now: the interval, not at once.
        assert_eq!(seconds_until_due(300, [None, Some(0)]), 300);
        assert_eq!(seconds_until_due(300, [None, Some(100)]), 200);
        assert_eq!(seconds_until_due(300, [None, Some(300)]), 0);
        // Fetched, never tried: the fetch counts.
        assert_eq!(seconds_until_due(300, [Some(100), None]), 200);
        // Fetched long ago, tried recently: the attempt counts; fetched
        // lately by the cache's clock, tried long ago: still the attempt.
        // That stamp was the future when the attempt was made, by a clock
        // since stepped back; in range again, it starts nothing over.
        assert_eq!(seconds_until_due(300, [Some(999), Some(100)]), 200);
        assert_eq!(seconds_until_due(300, [Some(100), Some(999)]), 0);
        assert_eq!(seconds_until_due(300, [Some(100), Some(300)]), 0);
        // A fetch stamped after now, by a clock since stepped back, is no
        // activity: one fetch now, then the interval from that attempt.
        assert_eq!(seconds_since(1_000, Some(2_000)), None);
        assert_eq!(seconds_since(1_000, Some(400)), Some(600));
        assert_eq!(seconds_since(1_000, None), None);
        // The interval is whole seconds and at least half a minute; zero
        // is off.
        assert_eq!(interval_seconds(Duration::from_secs(0)), None);
        assert_eq!(interval_seconds(Duration::from_millis(1_500)), Some(30));
        assert_eq!(interval_seconds(Duration::from_secs(31)), Some(31));
        assert_eq!(
            interval_seconds(Duration::from_secs(u64::MAX)),
            Some(i64::MAX)
        );
    }

    /// Over a real cache: a feed fetched long ago and one never fetched
    /// are due and a feed fetched lately is not; an attempt, whatever it
    /// returned, puts a feed off for the interval, and the wait is the
    /// shortest remaining one.
    #[test]
    fn a_feed_that_failed_waits_an_interval_like_one_that_succeeded() {
        use crate::feed::{format_local, FeedMeta};

        let dir = crate::testing::tempdir().unwrap();
        // The refresh below logs; not into anyone's real logs. The first
        // test to name a directory names it for the process: this one.
        let logs = dir.path().join("logs");
        assert_eq!(crate::log::use_log_dir(logs.clone()), logs);
        let cache = Cache::open_at(dir.path().join("test.tdkv")).unwrap();
        let feed = |name: &str| crate::config::FeedConfig {
            name: name.to_string(),
            url: format!("https://example.com/{name}"),
        };
        let feeds = [feed("stale"), feed("fresh"), feed("new")];
        let fetched = |url: &str, seconds_ago: i64| {
            cache.put_feed_meta(&FeedMeta {
                url: url.to_string(),
                title: String::new(),
                last_fetched: format_local(crate::civil::now_unix() - seconds_ago),
            })
        };
        fetched(&feeds[0].url, 1_000);
        fetched(&feeds[1].url, 10);
        let interval = Duration::from_secs(300);
        let mut attempts = HashMap::new();

        let (due, wait) = schedule(&cache, &feeds, interval, &attempts);
        assert_eq!(due, [0, 2]);
        assert_eq!(wait, Duration::from_secs(0));

        // Tried, and failed for all this knows: off for the interval.
        attempts.insert(feeds[0].url.clone(), Instant::now());
        attempts.insert(feeds[2].url.clone(), Instant::now());
        let (due, wait) = schedule(&cache, &feeds, interval, &attempts);
        assert!(due.is_empty(), "{due:?}");
        assert!((280..=290).contains(&wait.as_secs()), "{wait:?}");

        // The fresh feed's fetch stamped after now is no activity.
        fetched(&feeds[1].url, -3_600);
        let (due, wait) = schedule(&cache, &feeds, interval, &attempts);
        assert_eq!(due, [1]);
        assert_eq!(wait, Duration::from_secs(0));

        // The periodic refresh off: nothing due, the idle wait.
        assert_eq!(
            schedule(&cache, &feeds, Duration::ZERO, &attempts),
            (vec![], IDLE_WAIT)
        );

        // No feeds: the whole interval.
        assert_eq!(
            schedule(&cache, &[], interval, &attempts),
            (vec![], interval)
        );

        // A refresh records the attempt, a refused connection included.
        let refused = crate::config::FeedConfig {
            name: "refused".to_string(),
            url: "http://127.0.0.1:1/feed".to_string(),
        };
        let reports = refresh_feeds([&refused], &cache, &mut attempts);
        assert_eq!(reports.len(), 1);
        assert!(reports[0].error.is_some());
        assert!(attempts.contains_key(&refused.url));
        // Where the logs landed, not only where the path was set.
        assert!(logs.join("debug.log").is_file() && logs.join("news.log").is_file());
        let (due, _) = schedule(&cache, &[refused], interval, &attempts);
        assert!(due.is_empty(), "{due:?}");
    }
}
