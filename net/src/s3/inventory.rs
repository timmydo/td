//! Bounded inventory of td's digest-named release namespace.
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
fn page(xml: &str) -> Result<(Vec<String>, Option<String>)> {
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
            .strip_prefix("releases/")
            .ok_or("unexpected inventory namespace")?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("invalid remote release key".into());
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
        let mut keys = BTreeSet::new();
        let mut continuation: Option<String> = None;
        let mut tokens = BTreeSet::new();
        loop {
            let mut query = "list-type=2&max-keys=1000&prefix=releases%2F".to_string();
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
            let (part, next) =
                page(std::str::from_utf8(&bytes).map_err(|_| "inventory is not UTF-8")?)?;
            for key in part {
                if !keys.insert(key) || keys.len() > 200_000 {
                    return Err("duplicate or excessive S3 release inventory".into());
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
    pub(crate) fn prune(&self, key: &str, digest: &str, size: u64, delete: bool) -> Result<bool> {
        let (namespace, name) = key.split_once('/').ok_or("invalid cleanup key")?;
        if !matches!(namespace, "objects" | "releases")
            || name.len() != 64
            || !name
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("cleanup accepts only digest-named release indexes and objects".into());
        }
        let response = self.request("HEAD", key, &hash(b""), &[], None)?;
        if response.status().as_u16() == 404 {
            return Ok(false);
        }
        if response.status().as_u16() != 200
            || header(&response, "x-amz-meta-sha256")? != Some(digest)
            || header(&response, "content-length")?.and_then(|v| v.parse::<u64>().ok())
                != Some(size)
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
        let (keys, next) = page(&body).unwrap();
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
            assert!(page(&bad).is_err());
        }
        assert_eq!(encode("a+/=&b"), "a%2B%2F%3D%26b");
    }
}
