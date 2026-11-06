// What td's two HTTP clients, git-remote-https and td-curl, share: whom they
// trust, which proxy they use and where a redirect goes.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]
use std::io::Read;
use std::time::Duration;

use rustls::pki_types::{pem::PemObject, CertificateDer};
use ureq::{Proxy, ProxyProtocol};

pub(crate) const DEFAULT_CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_CA_BUNDLE: u64 = 16 * 1024 * 1024;
pub(crate) const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// Shared geometrically among a host's addresses, TLS handshake included.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a connection may send nothing. A server preparing a pack sends
/// keepalives, so this is a dead peer, not a slow one.
pub(crate) const STALL_TIMEOUT: Duration = Duration::from_secs(300);

/// An origin's scheme, host and port, with what writes one host's name two ways
/// made the same: credentials and headers stay with what this names.
pub(crate) fn origin_key(origin: &str, https: bool) -> String {
    let origin = origin.to_ascii_lowercase();
    let default = if https { ":443" } else { ":80" };
    match origin.strip_suffix(default) {
        Some(bare) => bare.to_string(),
        None => origin,
    }
}

/// A redirect's `Location` as an absolute URL, or the rest of a sentence saying
/// why not. A relative one holding a dot segment is refused rather than resolved.
pub(crate) fn absolute_location(
    origin: &str,
    https: bool,
    requested: &str,
    location: &str,
) -> Result<String, &'static str> {
    let lower = location.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(location.to_string());
    }
    // A scheme comes before any path or query, which may quote a URL of their own.
    if location
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|head| head.contains(':'))
    {
        return Err("redirected to a scheme this client does not speak");
    }
    let path = location.split('?').next().unwrap_or(location);
    if path
        .split('/')
        .any(|segment| segment == "." || segment == "..")
    {
        return Err("redirected with a dot segment, which is refused");
    }
    if let Some(rest) = location.strip_prefix("//") {
        let scheme = if https { "https" } else { "http" };
        return Ok(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        return Ok(format!("{origin}{location}"));
    }
    let resource = requested.split('?').next().unwrap_or(requested);
    if location.starts_with('?') {
        return Ok(format!("{resource}{location}"));
    }
    let directory = resource
        .rfind('/')
        .and_then(|at| resource.get(..=at))
        .unwrap_or(resource);
    Ok(format!("{directory}{location}"))
}

/// As Git and curl choose: `remote.*.proxy` or `http.proxy`, where an empty
/// value means none, then the scheme's variable and `all_proxy`. Plain HTTP
/// reads only the lower-case `http_proxy`, which a CGI request cannot set.
/// `no_proxy` wins over all of them.
pub(crate) fn select_proxy(
    https: bool,
    host: &str,
    configured: Option<String>,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let names: &[&str] = if https {
        &["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]
    } else {
        &["http_proxy", "all_proxy", "ALL_PROXY"]
    };
    let chosen = match configured {
        Some(value) => value,
        None => names
            .iter()
            .find_map(|name| lookup(name).filter(|value| !value.is_empty()))?,
    };
    if chosen.is_empty() {
        return None;
    }
    let bypass = lookup("no_proxy").or_else(|| lookup("NO_PROXY"));
    if bypass.is_some_and(|list| no_proxy_matches(&list, host)) {
        return None;
    }
    Some(chosen)
}

/// A host matches an entry it equals or is a subdomain of; `*` matches all.
pub(crate) fn no_proxy_matches(list: &str, host: &str) -> bool {
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    list.split(',').any(|entry| {
        let entry = entry
            .trim()
            .trim_start_matches('.')
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        !entry.is_empty()
            && (entry == "*"
                || host == entry
                || host
                    .strip_suffix(entry.as_str())
                    .is_some_and(|prefix| prefix.ends_with('.')))
    })
}

/// Text a server may have had a hand in, safe for a terminal.
pub(crate) fn printable(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|ch| {
            if ch.is_control() && ch != '\t' {
                '?'
            } else {
                ch
            }
        })
        .collect()
}

/// A proxy this client can use, or why not. The value may hold a password, so
/// no error says what it was.
pub(crate) fn usable_proxy(value: &str) -> Result<Proxy, String> {
    let proxy = Proxy::new(value)
        .map_err(|_| "the configured HTTP proxy is not a valid proxy URL".to_string())?;
    if !matches!(proxy.protocol(), ProxyProtocol::Http | ProxyProtocol::Https) {
        return Err("td's HTTP transport does not support a SOCKS proxy".to_string());
    }
    // Curl and this client's defaults differ, so saying one is the only safe way.
    if proxy.uri().port_u16().is_none() {
        return Err("the configured HTTP proxy names no port".to_string());
    }
    Ok(proxy)
}

pub(crate) fn load_tls(bundle: &str, required: bool) -> Result<ureq::tls::TlsConfig, String> {
    let bytes = match read_bundle(bundle) {
        Ok(bytes) => bytes,
        // A plain http remote has no use for the store. A redirect to https is
        // another origin, so it comes back here as required.
        Err(_) if !required => {
            return Ok(crate::fetchd::tls(
                ureq::tls::RootCerts::new_with_certs(&[]),
            ))
        }
        Err(error) => return Err(error),
    };
    let mut roots = Vec::new();
    let mut check = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&bytes) {
        let cert = cert.map_err(|e| format!("CA bundle {bundle}: {e}"))?;
        // One root rustls cannot parse is one root not trusted; the rest of a
        // pinned store stands, and an all-unusable one fails below.
        let der = ureq::tls::Certificate::from_der(cert.as_ref()).to_owned();
        if check.add(cert).is_ok() {
            roots.push(der);
        }
    }
    if roots.is_empty() {
        return Err(format!("CA bundle {bundle} holds no usable certificate"));
    }
    Ok(crate::fetchd::tls(ureq::tls::RootCerts::new_with_certs(
        &roots,
    )))
}

fn read_bundle(path: &str) -> Result<Vec<u8>, String> {
    // /etc/ssl's bundle is a symlink into the store, so a link is expected.
    let file = std::fs::File::open(path).map_err(|e| format!("CA bundle {path}: {e}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_CA_BUNDLE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("CA bundle {path}: {e}"))?;
    if bytes.len() as u64 > MAX_CA_BUNDLE {
        return Err(format!("CA bundle {path} is too large"));
    }
    Ok(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn a_location_resolves_against_the_request() {
        let origin = "https://a.test:8443";
        let requested = "https://a.test:8443/d/f?x=1";
        for (location, wanted) in [
            ("g", "https://a.test:8443/d/g"),
            ("?y=2", "https://a.test:8443/d/f?y=2"),
            ("/g", "https://a.test:8443/g"),
            ("//b.test/g", "https://b.test/g"),
            ("HTTP://b.test/g", "HTTP://b.test/g"),
            (
                "/login?next=https://a.test/",
                "https://a.test:8443/login?next=https://a.test/",
            ),
        ] {
            assert_eq!(
                absolute_location(origin, true, requested, location).unwrap(),
                wanted,
                "{location}"
            );
        }
        for refused in ["ftp://b.test/g", "mailto:a@b.test", "../g", "./g", "d/../g"] {
            assert!(
                absolute_location(origin, true, requested, refused).is_err(),
                "{refused}"
            );
        }
    }

    #[test]
    fn an_origin_key_folds_only_what_means_the_same() {
        let key = |origin, https| origin_key(origin, https);
        assert_eq!(key("https://A.test:443", true), key("https://a.test", true));
        assert_eq!(key("http://a.test:80", false), key("http://a.test", false));
        assert_ne!(
            key("https://a.test:8443", true),
            key("https://a.test", true)
        );
        assert_ne!(key("https://a.test:80", true), key("https://a.test", true));
        assert_ne!(key("http://a.test", false), key("https://a.test", true));
        assert_eq!(key("https://[::1]:443", true), "https://[::1]");
        assert_eq!(key("https://[::443]", true), "https://[::443]");
    }

    #[test]
    fn printable_steers_no_terminal() {
        assert_eq!(printable(b"a\x1b[31mb\nc\td"), "a?[31mb?c\td");
    }
}
