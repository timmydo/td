//! The network, as td-web uses it (DESIGN.md §4): one GET at a time
//! through the fetch service, which follows no redirect, and the
//! redirects td-web follows itself, so it knows the URL a document came
//! from and keeps a navigation's reach to the local network only on its
//! own host.

use td_url::{Reference, Url};

/// The most redirects one navigation follows.
pub const MAX_REDIRECTS: usize = 5;
/// The most bytes of a body taken; the service refuses one past it.
pub const MAX_DOCUMENT: u64 = 16 << 20;
/// What a navigation asks for: a page, else text, else a picture td-web
/// shows, else whatever there is.
pub const ACCEPT: &str = "text/html, application/xhtml+xml, text/plain;q=0.9, \
image/png;q=0.8, image/jpeg;q=0.8, */*;q=0.1";
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

/// One GET, following no redirect: `public` asks the service to refuse a
/// host on the local network as well as the ones it always refuses.
pub trait Fetcher: Send + Sync {
    fn get(&self, url: &str, public: bool) -> Result<Reply, String>;
}

/// The fetch service at `$XDG_RUNTIME_DIR/td-fetch/socket`.
pub struct Service;

impl Fetcher for Service {
    fn get(&self, url: &str, public: bool) -> Result<Reply, String> {
        let headers = [("accept", ACCEPT), ("user-agent", USER_AGENT)];
        let response = if public {
            td_fetch_client::get_public(url, &headers, Some(MAX_DOCUMENT))
        } else {
            td_fetch_client::get(url, &headers, Some(MAX_DOCUMENT), Some(0))
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
        let reply = fetcher.get(&url.text(), public)?;
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

#[cfg(test)]
pub mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;
    use std::sync::Mutex;

    /// Answers from a table, recording each request and its reach.
    #[derive(Default)]
    pub struct Table {
        pub answers: Vec<(String, Reply)>,
        pub asked: Mutex<Vec<(String, bool)>>,
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

        pub fn reach(&self) -> Vec<(String, bool)> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Fetcher for Table {
        fn get(&self, url: &str, public: bool) -> Result<Reply, String> {
            self.asked.lock().unwrap().push((url.into(), public));
            self.answers
                .iter()
                .find(|(known, _)| known == url)
                .map(|(_, reply)| reply.clone())
                .ok_or_else(|| format!("no route to {url}"))
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
}
