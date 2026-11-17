//! The network, as td-web uses it (DESIGN.md §4): one GET at a time
//! through the fetch service, which follows no redirect, and the
//! redirects td-web follows itself, so it knows the URL a document came
//! from and keeps a navigation's reach to the local network only on its
//! own host. A subresource (DESIGN.md §10, "Subresources") is always
//! sent `public` and refused at any hop that would take an `https`
//! page's subresource over `http`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use td_url::{Reference, Url};

/// The most redirects one navigation follows.
pub const MAX_REDIRECTS: usize = 5;
/// The most bytes of a body taken; the service refuses one past it.
pub const MAX_DOCUMENT: u64 = 16 << 20;
/// What a navigation asks for: a page, else text, else a picture td-web
/// shows, else whatever there is.
pub const ACCEPT: &str = "text/html, application/xhtml+xml, text/plain;q=0.9, \
image/png;q=0.8, image/jpeg;q=0.8, image/gif;q=0.8, image/webp;q=0.8, */*;q=0.1";
pub const USER_AGENT: &str = concat!("td-web/", env!("CARGO_PKG_VERSION"));
/// The statuses that redirect; any other is the document, a 304 or a 300
/// with a `location` included.
const REDIRECTS: &[u16] = &[301, 302, 303, 307, 308];

/// What the origin answered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Reply {
    pub status: u16,
    /// Names in lower case, in the order the origin sent them.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    /// The first value of `name` (lower case), if any.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What a GET asks: `public` asks the service to refuse a host on the
/// local network as well as the ones it always refuses; `accept` is sent
/// as the `accept` header; at most `limit` bytes of the body; and, with a
/// `deadline`, the service given until then for the exchange with the
/// origin, a subresource worth nothing after it (DESIGN.md §10, "Slots").
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ask {
    pub public: bool,
    pub accept: &'static str,
    pub limit: u64,
    pub deadline: Option<Instant>,
}

impl Ask {
    /// A navigation's hop.
    pub const fn navigation(public: bool) -> Self {
        Self {
            public,
            accept: ACCEPT,
            limit: MAX_DOCUMENT,
            deadline: None,
        }
    }
}

/// One GET, following no redirect.
pub trait Fetcher: Send + Sync {
    fn get(&self, url: &str, ask: Ask) -> Result<Reply, String>;
}

/// A fetcher the window, its navigations and their subresources share.
pub type Shared = Arc<dyn Fetcher>;

/// The fetch service at `$XDG_RUNTIME_DIR/td-fetch/socket`.
pub struct Service;

impl Fetcher for Service {
    fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
        let headers = [("accept", ask.accept), ("user-agent", USER_AGENT)];
        let within = match ask.deadline {
            Some(deadline) => match deadline.checked_duration_since(Instant::now()) {
                Some(left) if !left.is_zero() => Some(left),
                // Past it, nothing is asked.
                _ => return Err(LATE.into()),
            },
            None => None,
        };
        let response = if ask.public {
            td_fetch_client::get_public_within(url, &headers, Some(ask.limit), within)
        } else {
            td_fetch_client::get(url, &headers, Some(ask.limit), Some(0))
        };
        response
            .map(|response| Reply {
                status: response.status,
                headers: response.headers,
                body: response.body,
            })
            .map_err(|why| why.to_string())
    }
}

/// A document as it arrived: the URL that answered, after redirects, and
/// the fragment the last redirect named, which wins over the one the
/// navigation named (WHATWG's fetch).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Loaded {
    pub url: Url,
    pub fragment: Option<String>,
    pub reply: Reply,
}

/// Fetches `url`, a navigation the person chose, following at most
/// `MAX_REDIRECTS` redirects: a hop to another host is asked `public`,
/// and every hop after it too.
pub fn navigate(fetcher: &dyn Fetcher, url: Url) -> Result<Loaded, String> {
    let mut url = url;
    let mut fragment = None;
    let mut public = false;
    let mut hops = 0;
    loop {
        let reply = fetcher.get(&url.text(), Ask::navigation(public))?;
        let location = match reply.header("location") {
            Some(location) if REDIRECTS.contains(&reply.status) => location,
            _ => {
                return Ok(Loaded {
                    url,
                    fragment,
                    reply,
                })
            }
        };
        hops += 1;
        if hops > MAX_REDIRECTS {
            return Err(format!(
                "{} redirected more than {MAX_REDIRECTS} times",
                url.text()
            ));
        }
        let next = match td_url::resolve(&url, location) {
            Ok(Reference::Fetch {
                url: next,
                fragment: named,
            }) => {
                if named.is_some() {
                    fragment = named;
                }
                next
            }
            Ok(Reference::Other { scheme }) => {
                return Err(format!(
                    "{} redirected to a {scheme} address, which td-web does not open",
                    url.text()
                ))
            }
            Err(why) => {
                return Err(format!(
                    "{} redirected to an address td-web does not take: {why}",
                    url.text()
                ))
            }
        };
        public = public || !next.same_host(&url);
        url = next;
    }
}

/// What the service is not asked once a request's deadline has passed.
const LATE: &str = "past the deadline";

/// Why a subresource was not had.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    /// The service refused it, or the fetch failed.
    Fetch,
    /// A hop over `http` for an `https` page: mixed content.
    Mixed,
    /// Past `MAX_REDIRECTS`, or to a `location` td-web does not take.
    Redirect,
    /// Its page stopped, or its deadline passed, before a hop: no more
    /// is asked.
    Stopped,
}

/// Fetches `url`, a subresource of a page that is `https` or not, as
/// `ask` says but always `public`, following at most `MAX_REDIRECTS`
/// redirects, any hop to `http` under an `https` page refused before it
/// is asked, and no hop asked once `stopped` is set or `ask`'s deadline
/// has passed (DESIGN.md §10, "Subresources" and "Slots").
pub fn subresource(
    fetcher: &dyn Fetcher,
    url: Url,
    https: bool,
    ask: Ask,
    stopped: &AtomicBool,
) -> Result<Loaded, Failure> {
    let ask = Ask {
        public: true,
        ..ask
    };
    let mut url = url;
    for _ in 0..=MAX_REDIRECTS {
        let late = ask
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        if late || stopped.load(Ordering::SeqCst) {
            return Err(Failure::Stopped);
        }
        if https && !url.https {
            return Err(Failure::Mixed);
        }
        let reply = fetcher.get(&url.text(), ask).map_err(|_| Failure::Fetch)?;
        let location = match reply.header("location") {
            Some(location) if REDIRECTS.contains(&reply.status) => location,
            _ => {
                return Ok(Loaded {
                    url,
                    fragment: None,
                    reply,
                })
            }
        };
        url = match td_url::resolve(&url, location) {
            Ok(Reference::Fetch { url, .. }) => url,
            Ok(Reference::Other { .. }) | Err(_) => return Err(Failure::Redirect),
        };
    }
    Err(Failure::Redirect)
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::sync::Mutex;

    /// Answers from a table, recording each request and what it asked.
    #[derive(Default)]
    pub struct Table {
        pub answers: Vec<(String, Reply)>,
        pub asked: Mutex<Vec<(String, Ask)>>,
    }

    impl Table {
        pub fn reply(
            mut self,
            url: &str,
            status: u16,
            headers: &[(&str, &str)],
            body: &[u8],
        ) -> Self {
            self.answers.push((
                url.into(),
                Reply {
                    status,
                    headers: headers
                        .iter()
                        .map(|(name, value)| (name.to_string(), value.to_string()))
                        .collect(),
                    body: body.to_vec(),
                },
            ));
            self
        }

        pub fn page(self, url: &str, content_type: &str, body: &str) -> Self {
            self.reply(url, 200, &[("content-type", content_type)], body.as_bytes())
        }

        pub fn redirect(self, url: &str, location: &str) -> Self {
            self.reply(url, 302, &[("location", location)], b"")
        }

        pub fn sheet(self, url: &str, body: &str) -> Self {
            self.page(url, "text/css", body)
        }

        pub fn reach(&self) -> Vec<(String, bool)> {
            let asked = self.asked.lock().unwrap();
            asked
                .iter()
                .map(|(url, ask)| (url.clone(), ask.public))
                .collect()
        }

        /// The URLs asked, in order.
        pub fn urls(&self) -> Vec<String> {
            self.reach().into_iter().map(|(url, _)| url).collect()
        }
    }

    /// A corpus captured to scratch for the host measurements (DESIGN.md
    /// §10, increment 8), never committed: `pages.tsv` names each page and
    /// its URL; `sub.tsv` each answer captured, a URL, its status, type,
    /// location, file under `sub/` and the seconds it took.
    pub struct Corpus {
        pub pages: Vec<(String, String)>,
        pub table: Table,
        /// Each answer's seconds, which a replay sleeps.
        pub seconds: Vec<(String, f64)>,
        pub replay: bool,
    }

    impl Corpus {
        pub fn read(dir: &std::path::Path) -> Option<Self> {
            let pages = std::fs::read_to_string(dir.join("pages.tsv")).ok()?;
            let pages = pages
                .lines()
                .filter_map(|line| line.split_once('\t'))
                .map(|(name, url)| (name.to_string(), url.to_string()))
                .collect();
            let mut table = Table::default();
            let mut seconds = Vec::new();
            let index = std::fs::read_to_string(dir.join("sub.tsv")).unwrap_or_default();
            for line in index.lines() {
                let fields: Vec<&str> = line.split('\t').collect();
                let [url, status, kind, location, file, took] = fields[..] else {
                    continue;
                };
                let status: u16 = status.parse().unwrap_or(0);
                if status == 0 {
                    continue;
                }
                let body = std::fs::read(dir.join("sub").join(file)).unwrap_or_default();
                let mut headers = vec![("content-type", kind)];
                if !location.is_empty() {
                    headers.push(("location", location));
                }
                table = table.reply(url, status, &headers, &body);
                seconds.push((url.to_string(), took.parse().unwrap_or(0.0)));
            }
            Some(Self {
                pages,
                table,
                seconds,
                replay: std::env::var_os("TD_WEB_REPLAY").is_some(),
            })
        }

        /// Writes the URLs asked for and not captured to `missing.txt`,
        /// for the capture to fetch.
        pub fn missing(&self, dir: &std::path::Path) -> usize {
            let mut missing: Vec<String> = self
                .table
                .urls()
                .into_iter()
                .filter(|url| !self.table.answers.iter().any(|(known, _)| known == url))
                .collect();
            missing.sort();
            missing.dedup();
            std::fs::write(dir.join("missing.txt"), missing.join("\n")).unwrap();
            missing.len()
        }
    }

    impl Fetcher for Corpus {
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            if self.replay {
                if let Some((_, took)) = self.seconds.iter().find(|(known, _)| known == url) {
                    std::thread::sleep(std::time::Duration::from_secs_f64(*took));
                }
            }
            self.table.get(url, ask)
        }
    }

    impl Fetcher for Table {
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            self.asked.lock().unwrap().push((url.into(), ask));
            let reply = self
                .answers
                .iter()
                .find(|(known, _)| known == url)
                .map(|(_, reply)| reply.clone())
                .ok_or_else(|| format!("no route to {url}"))?;
            // The service refuses a body past the request's limit.
            if reply.body.len() as u64 > ask.limit {
                return Err(format!("{url}: the body is past {} bytes", ask.limit));
            }
            Ok(reply)
        }
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn a_redirect_is_followed_and_the_document_keeps_the_url_that_answered() {
        let table = Table::default()
            .redirect("http://192.0.2.1/", "https://192.0.2.1/setup/#top")
            .redirect("https://192.0.2.1/setup/", "index.html")
            .page("https://192.0.2.1/setup/index.html", "text/html", "<p>hi");
        let loaded = navigate(&table, url("http://192.0.2.1/")).unwrap();
        assert_eq!(loaded.url.text(), "https://192.0.2.1/setup/index.html");
        assert_eq!(loaded.reply.body, b"<p>hi");
        assert_eq!(
            loaded.fragment.as_deref(),
            Some("top"),
            "kept past a hop naming none"
        );
        // The same host keeps its reach, whatever the scheme.
        assert!(table.reach().iter().all(|(_, public)| !public));
    }

    #[test]
    fn a_hop_to_another_host_and_every_one_after_it_is_public() {
        let table = Table::default()
            .redirect("https://example.org/", "https://other.example/a")
            .redirect("https://other.example/a", "https://example.org/b")
            .page("https://example.org/b", "text/plain", "x");
        navigate(&table, url("https://example.org/")).unwrap();
        let reach: Vec<bool> = table.reach().iter().map(|(_, public)| *public).collect();
        assert_eq!(reach, [false, true, true]);
    }

    #[test]
    fn too_many_redirects_or_a_foreign_scheme_is_an_error() {
        let mut table = Table::default();
        for n in 0..=MAX_REDIRECTS {
            table = table.redirect(&format!("https://example.org/{n}"), &format!("/{}", n + 1));
        }
        let err = navigate(&table, url("https://example.org/0")).unwrap_err();
        assert!(err.contains("more than 5"), "{err}");
        let table = Table::default().redirect("https://example.org/", "mailto:a@example.org");
        let err = navigate(&table, url("https://example.org/")).unwrap_err();
        assert!(err.contains("mailto"), "{err}");
    }

    #[test]
    fn only_a_redirect_status_is_followed() {
        for status in [300, 304, 399] {
            let table = Table::default().reply(
                "https://example.org/",
                status,
                &[("location", "/elsewhere")],
                b"",
            );
            let loaded = navigate(&table, url("https://example.org/")).unwrap();
            assert_eq!(loaded.reply.status, status);
            assert_eq!(table.reach().len(), 1);
        }
    }

    const SHEET: Ask = Ask {
        public: false,
        accept: "text/css,*/*;q=0.1",
        limit: 1 << 20,
        deadline: None,
    };
    static GO: AtomicBool = AtomicBool::new(false);

    /// Answers from a table, setting `stop` once `at` is asked.
    struct Stopping<'a> {
        table: Table,
        at: &'a str,
        stop: &'a AtomicBool,
    }

    impl Fetcher for Stopping<'_> {
        fn get(&self, url: &str, ask: Ask) -> Result<Reply, String> {
            if url == self.at {
                self.stop.store(true, Ordering::SeqCst);
            }
            self.table.get(url, ask)
        }
    }

    #[test]
    fn a_subresource_asks_no_hop_once_stopped_or_past_its_deadline() {
        let table = || {
            Table::default()
                .redirect("https://example.org/a.css", "/b.css")
                .redirect("https://example.org/b.css", "/c.css")
                .sheet("https://example.org/c.css", "p{}")
        };
        // Stopped while b is asked: c is never asked.
        let stop = AtomicBool::new(false);
        let stopping = Stopping {
            table: table(),
            at: "https://example.org/b.css",
            stop: &stop,
        };
        let got = subresource(
            &stopping,
            url("https://example.org/a.css"),
            true,
            SHEET,
            &stop,
        );
        assert_eq!(got.err(), Some(Failure::Stopped));
        assert_eq!(
            stopping.table.urls(),
            ["https://example.org/a.css", "https://example.org/b.css"]
        );
        // Past its deadline, nothing is asked; before it, each hop asks
        // with it.
        let table = table();
        let past = Ask {
            deadline: Some(Instant::now()),
            ..SHEET
        };
        let got = subresource(&table, url("https://example.org/a.css"), true, past, &GO);
        assert_eq!(got.err(), Some(Failure::Stopped));
        assert!(table.urls().is_empty());
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let within = Ask {
            deadline: Some(deadline),
            ..SHEET
        };
        subresource(&table, url("https://example.org/a.css"), true, within, &GO).unwrap();
        let asked = table.asked.lock().unwrap();
        assert_eq!(asked.len(), 3);
        assert!(asked.iter().all(|(_, ask)| ask.deadline == Some(deadline)));
    }

    #[test]
    fn a_subresource_is_public_and_asks_what_it_says() {
        let table = Table::default()
            .redirect("https://192.0.2.1/a.css", "/b.css")
            .sheet("https://192.0.2.1/b.css", "p{}");
        let loaded = subresource(&table, url("https://192.0.2.1/a.css"), true, SHEET, &GO).unwrap();
        assert_eq!(loaded.url.text(), "https://192.0.2.1/b.css");
        let asked = table.asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        // Public at every hop, even on its own host, as the sheet asks.
        assert!(asked.iter().all(|(_, ask)| *ask
            == Ask {
                public: true,
                ..SHEET
            }));
    }

    #[test]
    fn mixed_content_is_refused_at_whichever_hop_it_appears() {
        let table = Table::default()
            .redirect("https://example.org/a.css", "http://example.org/b.css")
            .sheet("http://example.org/b.css", "p{}");
        assert_eq!(
            subresource(&table, url("https://example.org/a.css"), true, SHEET, &GO).err(),
            Some(Failure::Mixed)
        );
        assert_eq!(table.urls(), ["https://example.org/a.css"], "b never asked");
        assert_eq!(
            subresource(&table, url("http://example.org/b.css"), true, SHEET, &GO).err(),
            Some(Failure::Mixed)
        );
        // An http page's http subresource is no mixed content.
        assert!(subresource(&table, url("https://example.org/a.css"), false, SHEET, &GO).is_ok());
        let mut table = Table::default();
        for n in 0..=MAX_REDIRECTS {
            table = table.redirect(&format!("https://example.org/{n}"), &format!("/{}", n + 1));
        }
        assert_eq!(
            subresource(&table, url("https://example.org/0"), true, SHEET, &GO).err(),
            Some(Failure::Redirect)
        );
        assert_eq!(
            subresource(&table, url("https://example.org/none"), true, SHEET, &GO).err(),
            Some(Failure::Fetch)
        );
    }
}
