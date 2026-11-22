//! Bounded inventory of td's digest-named namespaces, and their cleanup.
use super::*;
use std::collections::BTreeSet;

fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
fn field<'a>(xml: &'a str, name: &str) -> Result<Option<&'a str>> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let Some((_, rest)) = xml.split_once(&open) else {
        return Ok(None);
    };
    let (value, tail) = rest
        .split_once(&close)
        .ok_or("truncated S3 inventory field")?;
    if value.contains('<') || tail.contains(&open) {
        return Err("ambiguous S3 inventory field".into());
    }
    Ok(Some(value))
}
fn unescape(value: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = value;
    while let Some((before, after)) = rest.split_once('&') {
        out.push_str(before);
        let (entity, tail) = after.split_once(';').ok_or("invalid inventory entity")?;
        out.push(match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => return Err("unsupported inventory entity".into()),
        });
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}
/// The namespaces `digest_keys` lists: each holds only `NAMESPACE/SHA256`.
const LISTED: &[&str] = &["releases", "sources"];

fn digest_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// An ISO basename as `release sign-iso` admits it (`release/iso.rs` holds
/// its signer to this one rule).
pub(crate) fn iso_name(name: &str) -> bool {
    name.len() <= 128
        && name.ends_with(".iso")
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// The keys cleanup may delete, within the caller's `scope` of namespaces:
/// digest-named release indexes, objects and mirrored sources, and a retired
/// ISO image or its signature record.
fn cleanup_key(key: &str, scope: &[&str]) -> bool {
    let Some((namespace, rest)) = key.split_once('/') else {
        return false;
    };
    if !scope.contains(&namespace) {
        return false;
    }
    match namespace {
        "objects" | "releases" | "sources" => digest_name(rest),
        "iso" => match rest.split_once('/') {
            Some(("signatures", file)) => file.strip_suffix(".sig").is_some_and(digest_name),
            Some((digest, name)) => digest_name(digest) && iso_name(name),
            None => false,
        },
        _ => false,
    }
}

fn page(xml: &str, namespace: &str) -> Result<(Vec<String>, Option<String>)> {
    if xml.contains("<!")
        || !xml.contains("<ListBucketResult")
        || !xml.trim_end().ends_with("</ListBucketResult>")
    {
        return Err("invalid S3 inventory XML".into());
    }
    let truncated = match field(xml, "IsTruncated")? {
        Some("true") => true,
        Some("false") => false,
        _ => return Err("invalid S3 truncation field".into()),
    };
    let count = field(xml, "KeyCount")?
        .ok_or("missing S3 key count")?
        .parse::<usize>()
        .map_err(|_| "invalid S3 key count")?;
    if count > 1000 {
        return Err("S3 inventory page exceeds 1000 keys".into());
    }
    let mut keys = Vec::new();
    let mut rest = xml;
    while let Some((_, after)) = rest.split_once("<Contents>") {
        let (entry, tail) = after
            .split_once("</Contents>")
            .ok_or("truncated S3 inventory entry")?;
        let key = field(entry, "Key")?.ok_or("missing inventory key")?;
        let digest = key
            .strip_prefix(namespace)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or("unexpected inventory namespace")?;
        if !digest_name(digest) {
            return Err("invalid remote digest key".into());
        }
        keys.push(key.to_string());
        rest = tail;
        if keys.len() > 1000 {
            return Err("too many S3 inventory entries".into());
        }
    }
    if keys.len() != count {
        return Err("S3 inventory key count mismatch".into());
    }
    let next = field(xml, "NextContinuationToken")?
        .map(unescape)
        .transpose()?;
    if truncated != next.is_some()
        || next
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 8192 || s.chars().any(char::is_control))
        || (truncated && keys.is_empty())
    {
        return Err("invalid S3 continuation token".into());
    }
    Ok((keys, next))
}
impl Client {
    pub(crate) fn release_keys(&self) -> Result<BTreeSet<String>> {
        self.digest_keys("releases")
    }
    /// Every `NAMESPACE/SHA256` key, for a namespace in `LISTED`.
    pub(crate) fn digest_keys(&self, namespace: &str) -> Result<BTreeSet<String>> {
        if !LISTED.contains(&namespace) {
            return Err("unlisted S3 inventory namespace".into());
        }
        let mut keys = BTreeSet::new();
        let mut continuation: Option<String> = None;
        let mut tokens = BTreeSet::new();
        loop {
            let mut query = format!("list-type=2&max-keys=1000&prefix={namespace}%2F");
            if let Some(token) = &continuation {
                query = format!("continuation-token={}&{query}", encode(token));
            }
            let mut response = self.request_query("GET", "", &query, &hash(b""), &[], None)?;
            if response.status().as_u16() != 200 {
                return Err(format!("S3 inventory: HTTP {}", response.status().as_u16()));
            }
            let mut bytes = Vec::new();
            response
                .body_mut()
                .as_reader()
                .take(2 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| "S3 inventory read failed")?;
            if bytes.len() > 2 * 1024 * 1024 {
                return Err("S3 inventory response too large".into());
            }
            let (part, next) = page(
                std::str::from_utf8(&bytes).map_err(|_| "inventory is not UTF-8")?,
                namespace,
            )?;
            for key in part {
                if !keys.insert(key) || keys.len() > 200_000 {
                    return Err("duplicate or excessive S3 inventory".into());
                }
            }
            match next {
                None => return Ok(keys),
                Some(token) => {
                    if !tokens.insert(token.clone()) {
                        return Err("S3 inventory continuation loop".into());
                    }
                    continuation = Some(token);
                }
            }
        }
    }
    /// Delete `key` (or, without `delete`, only check it) if it lies in
    /// `scope` and its upload metadata names `digest` and, when given,
    /// `size`; absent is `Ok(false)`.
    pub(crate) fn prune(
        &self,
        key: &str,
        scope: &[&str],
        digest: &str,
        size: Option<u64>,
        delete: bool,
    ) -> Result<bool> {
        if !cleanup_key(key, scope) {
            return Err("cleanup accepts only digest-named keys and retired ISO files".into());
        }
        let response = self.request("HEAD", key, &hash(b""), &[], None)?;
        if response.status().as_u16() == 404 {
            return Ok(false);
        }
        if response.status().as_u16() != 200
            || header(&response, "x-amz-meta-sha256")? != Some(digest)
            || size.is_some_and(|size| {
                header(&response, "content-length")
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<u64>().ok())
                    != Some(size)
            })
        {
            return Err(format!(
                "refusing cleanup of mismatched remote object {key}"
            ));
        }
        let etag = header(&response, "etag")?
            .ok_or("cleanup HEAD omitted ETag")?
            .to_string();
        if etag.len() > 256
            || !etag.starts_with('"')
            || !etag.ends_with('"')
            || etag.bytes().any(|b| !(33..=126).contains(&b))
        {
            return Err("invalid cleanup ETag".into());
        }
        if delete {
            let response = self.request("DELETE", key, &hash(b""), &[("if-match", &etag)], None)?;
            if !matches!(response.status().as_u16(), 200 | 204 | 404) {
                return Err(format!(
                    "S3 cleanup {key}: HTTP {}",
                    response.status().as_u16()
                ));
            }
        }
        Ok(true)
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    #[test]
    fn listing_is_bounded_and_cannot_hide_a_truncated_page() {
        let body=format!("<ListBucketResult><KeyCount>1</KeyCount><IsTruncated>true</IsTruncated><Contents><Key>releases/{}</Key></Contents><NextContinuationToken>a&amp;b</NextContinuationToken></ListBucketResult>","a".repeat(64));
        let (keys, next) = page(&body, "releases").unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(next.as_deref(), Some("a&b"));
        for bad in [
            body.replace("<IsTruncated>true</IsTruncated>", ""),
            body.replace("<KeyCount>1", "<KeyCount>2"),
            body.replace("<NextContinuationToken>a&amp;b</NextContinuationToken>", ""),
            body.replace("releases/", "objects/"),
            body.replace("a&amp;b", "&unknown;"),
            body.replace("</Contents>", ""),
        ] {
            assert!(page(&bad, "releases").is_err());
        }
        assert!(page(&body.replace("releases/", "sources/"), "sources").is_ok());
        assert!(page(&body, "sources").is_err());
        assert_eq!(encode("a+/=&b"), "a%2B%2F%3D%26b");
    }

    #[test]
    fn cleanup_touches_only_digest_keys_and_retired_iso_files() {
        const ALL: &[&str] = &["objects", "releases", "sources", "iso"];
        let d = "a".repeat(64);
        for good in [
            format!("objects/{d}"),
            format!("releases/{d}"),
            format!("sources/{d}"),
            format!("iso/{d}/td-install-x86-64.iso"),
            format!("iso/signatures/{d}.sig"),
        ] {
            assert!(cleanup_key(&good, ALL), "{good}");
        }
        // Each caller reaches only its own namespaces.
        assert!(!cleanup_key(
            &format!("sources/{d}"),
            &["objects", "releases"]
        ));
        assert!(!cleanup_key(
            &format!("iso/signatures/{d}.sig"),
            &["sources"]
        ));
        for bad in [
            format!("roots/{d}"),
            format!("heads/{d}"),
            "index.html".to_string(),
            format!("sources/{}", "A".repeat(64)),
            format!("iso/{d}/../index.html"),
            format!("iso/{d}/td.img"),
            format!("iso/signatures/{d}"),
            format!("iso/{d}"),
            format!("sources/{d}/x"),
        ] {
            assert!(!cleanup_key(&bad, ALL), "{bad}");
        }
    }
}
