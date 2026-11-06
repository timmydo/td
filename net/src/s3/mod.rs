//! Bounded path-style S3 uploads. Credentials never enter argv or diagnostics.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented,
    clippy::indexing_slicing
)]

#[cfg(test)]
pub(crate) mod tests;

use crate::{sig, td_fs};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use td_primitives::{
    hmac,
    sha::{Digest, Sha256},
};

type Result<T> = std::result::Result<T, String>;
pub(crate) const IMMUTABLE: &str = "public, max-age=31536000, immutable";
pub(crate) const NO_STORE: &str = "no-store";
pub(crate) const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;
const ATTEMPTS: usize = 3;

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| e.to_string())
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    sig::to_hex(&Sha256::digest(bytes))
}

struct Credentials {
    access: String,
    secret: String,
    token: Option<String>,
}
impl Credentials {
    fn load(path: &Path, profile: &str) -> Result<Self> {
        let (mut file, meta) = io(td_fs::open_real_file(path, "S3 credentials"))?;
        if meta.uid() != io(fs::metadata("/proc/self"))?.uid()
            || meta.mode() & 0o077 != 0
            || meta.len() > 65536
        {
            return Err(
                "S3 credentials must be an owned private regular file of at most 64 KiB".into(),
            );
        }
        let mut bytes = Vec::new();
        io(file.by_ref().take(65537).read_to_end(&mut bytes))?;
        if bytes.len() > 65536 {
            return Err("S3 credentials too large".into());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "S3 credentials are not UTF-8")?;
        Self::parse(text, profile)
    }
    fn parse(text: &str, profile: &str) -> Result<Self> {
        if profile.is_empty()
            || profile.len() > 128
            || !profile
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err("invalid S3 credentials profile".into());
        }
        let mut selected = false;
        let mut found = false;
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                selected = section == profile;
                if selected && found {
                    return Err("duplicate S3 credentials profile".into());
                }
                found |= selected;
            } else if selected {
                let (name, value) = line.split_once('=').ok_or("invalid S3 credentials entry")?;
                let name = name.trim();
                let value = value.trim();
                if !matches!(
                    name,
                    "aws_access_key_id" | "aws_secret_access_key" | "aws_session_token"
                ) || value.is_empty()
                    || value.len() > 16384
                    || !value.bytes().all(|b| (33..=126).contains(&b))
                    || fields.insert(name, value).is_some()
                {
                    return Err("invalid or duplicate S3 credentials field".into());
                }
            }
        }
        let access = fields
            .get("aws_access_key_id")
            .ok_or("missing S3 access key")?
            .to_string();
        if access.len() > 128 || !access.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err("invalid S3 access key format".into());
        }
        Ok(Self {
            access,
            secret: fields
                .get("aws_secret_access_key")
                .ok_or("missing S3 secret key")?
                .to_string(),
            token: fields.get("aws_session_token").map(|s| s.to_string()),
        })
    }
}

fn timestamp(seconds: u64) -> Result<String> {
    if seconds > 253402300799 {
        return Err("S3 clock outside supported range".into());
    }
    let (year, month, day) = crate::utc::civil(seconds / 86400);
    Ok(format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    ))
}

fn authorization(
    credentials: &Credentials,
    method: &str,
    path: &str,
    region: &str,
    stamp: &str,
    headers: &BTreeMap<String, String>,
    digest: &str,
) -> Result<String> {
    let date = stamp.get(..8).ok_or("invalid S3 signing date")?;
    let scope = format!("{date}/{region}/s3/aws4_request");
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}:{}\n",
                v.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
            )
        })
        .collect();
    let signed = headers
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed}\n{digest}");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hash(canonical.as_bytes())
    );
    let date_key = hmac::mac::<Sha256>(
        format!("AWS4{}", credentials.secret).as_bytes(),
        &[date.as_bytes()],
    );
    let region_key = hmac::mac::<Sha256>(&date_key, &[region.as_bytes()]);
    let service_key = hmac::mac::<Sha256>(&region_key, &[b"s3"]);
    let signing_key = hmac::mac::<Sha256>(&service_key, &[b"aws4_request"]);
    let signature = sig::to_hex(&hmac::mac::<Sha256>(&signing_key, &[to_sign.as_bytes()]));
    Ok(format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed},Signature={signature}",
        credentials.access
    ))
}

fn key_path(bucket: &str, key: &str) -> Result<String> {
    if key.is_empty()
        || key.len() > 1024
        || key.chars().any(char::is_control)
        || key
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || key.contains('\\')
    {
        return Err("invalid S3 object key".into());
    }
    let mut path = format!("/{bucket}/");
    for byte in key.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            path.push(byte as char);
        } else {
            path.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(path)
}

pub(crate) struct Client {
    endpoint: String,
    host: String,
    region: String,
    bucket: String,
    credentials: Credentials,
    agent: ureq::Agent,
    deadline: Instant,
    pub(crate) uploaded: u64,
    pub(crate) reused: u64,
}

pub(crate) struct Remote {
    pub(crate) bytes: Vec<u8>,
    pub(crate) etag: String,
}

impl Client {
    pub(crate) fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        credentials: &Path,
        profile: &str,
    ) -> Result<Self> {
        let endpoint = endpoint.trim_end_matches('/');
        let uri: ureq::http::Uri = endpoint.parse().map_err(|_| "invalid S3 endpoint")?;
        let loopback = matches!(uri.host(), Some("127.0.0.1") | Some("[::1]"));
        if !(uri.scheme_str() == Some("https") || (uri.scheme_str() == Some("http") && loopback))
            || uri.host().is_none()
            || uri.path() != "/"
            || endpoint.contains(['@', '?', '#', '\\'])
        {
            return Err("S3 endpoint must be an HTTPS origin (numeric loopback HTTP is allowed for testing)".into());
        }
        if !(3..=63).contains(&bucket.len())
            || !bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || bucket.starts_with('-')
            || bucket.ends_with('-')
        {
            return Err(
                "S3 bucket must contain 3–63 lowercase letters, digits or internal hyphens".into(),
            );
        }
        if region.is_empty()
            || region.len() > 64
            || !region
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("invalid S3 region (use auto for R2)".into());
        }
        let deadline = Instant::now() + Duration::from_secs(3600);
        Ok(Self {
            endpoint: endpoint.into(),
            host: uri.authority().ok_or("S3 host missing")?.to_string(),
            region: region.into(),
            bucket: bucket.into(),
            credentials: Credentials::load(credentials, profile)?,
            agent: crate::http::agent_before(deadline, false)?,
            deadline,
            uploaded: 0,
            reused: 0,
        })
    }
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }
    pub(crate) fn remaining(&self) -> Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| "S3 operation exceeded one hour".into())
    }
    fn request(
        &self,
        method: &str,
        key: &str,
        digest: &str,
        extra: &[(&str, &str)],
        mut body: Option<(&mut File, u64)>,
    ) -> Result<ureq::http::Response<ureq::Body>> {
        let path = key_path(&self.bucket, key)?;
        let url = format!("{}{path}", self.endpoint);
        for attempt in 0..ATTEMPTS {
            let remaining = self.remaining()?;
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| "S3 clock before epoch")?
                .as_secs();
            let stamp = timestamp(seconds)?;
            let mut headers = BTreeMap::from([
                ("host".into(), self.host.clone()),
                ("x-amz-date".into(), stamp.clone()),
                ("x-amz-content-sha256".into(), digest.into()),
            ]);
            if let Some(token) = &self.credentials.token {
                headers.insert("x-amz-security-token".into(), token.clone());
            }
            for (name, value) in extra {
                headers.insert((*name).into(), (*value).into());
            }
            if let Some((_, size)) = &body {
                headers.insert("content-length".into(), size.to_string());
            }
            let auth = authorization(
                &self.credentials,
                method,
                &path,
                &self.region,
                &stamp,
                &headers,
                digest,
            )?;
            // Do not format ureq errors: credentials and signed headers are private.
            let result = if let Some((file, size)) = body.as_mut() {
                io(file.seek(SeekFrom::Start(0)))?;
                let mut request = self.agent.put(&url);
                for (name, value) in &headers {
                    request = request.header(name, value);
                }
                let mut reader = file.take(*size);
                request
                    .header("authorization", &auth)
                    .config()
                    .timeout_global(Some(remaining))
                    .timeout_per_write(Some(Duration::from_secs(30)))
                    .timeout_per_read(Some(Duration::from_secs(60)))
                    .build()
                    .send(ureq::SendBody::from_reader(&mut reader))
            } else {
                let mut request = if method == "HEAD" {
                    self.agent.head(&url)
                } else {
                    self.agent.get(&url)
                };
                for (name, value) in &headers {
                    request = request.header(name, value);
                }
                request
                    .header("authorization", &auth)
                    .config()
                    .timeout_global(Some(remaining))
                    .timeout_per_read(Some(Duration::from_secs(60)))
                    .build()
                    .call()
            };
            match result {
                Ok(response)
                    if !matches!(
                        response.status().as_u16(),
                        408 | 429 | 500 | 502 | 503 | 504
                    ) =>
                {
                    return Ok(response)
                }
                _ if attempt + 1 < ATTEMPTS => std::thread::sleep(
                    Duration::from_millis(200 * (attempt as u64 + 1)).min(self.remaining()?),
                ),
                _ => return Err(format!("S3 {method} failed after {ATTEMPTS} attempts")),
            }
        }
        Err("S3 attempt limit reached".into())
    }
    pub(crate) fn get(&self, key: &str, limit: u64) -> Result<Option<Remote>> {
        let mut response = self.request("GET", key, &hash(b""), &[], None)?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if response.status().as_u16() != 200 {
            return Err(format!("S3 GET {key}: HTTP {}", response.status().as_u16()));
        }
        let etag = header(&response, "etag")?
            .ok_or("S3 response omitted ETag")?
            .to_string();
        if etag.len() > 256
            || !etag.starts_with('"')
            || !etag.ends_with('"')
            || etag.bytes().any(|b| !(33..=126).contains(&b))
        {
            return Err("invalid S3 ETag".into());
        }
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "S3 response body failed")?;
        if bytes.len() as u64 > limit {
            return Err("S3 response exceeds metadata bound".into());
        }
        Ok(Some(Remote { bytes, etag }))
    }
    fn matches(&self, key: &str, digest: &str, size: u64, cache: &str) -> Result<bool> {
        let response = self.request("HEAD", key, &hash(b""), &[], None)?;
        match response.status().as_u16() {
            404 => Ok(false),
            200 => {
                if header(&response, "content-length")?.and_then(|v| v.parse::<u64>().ok())
                    != Some(size)
                    || header(&response, "x-amz-meta-sha256")? != Some(digest)
                    || header(&response, "cache-control")? != Some(cache)
                {
                    return Err(format!(
                        "S3 immutable object differs or lacks upload metadata: {key}"
                    ));
                }
                Ok(true)
            }
            code => Err(format!("S3 HEAD {key}: HTTP {code}")),
        }
    }
    pub(crate) fn immutable(
        &mut self,
        key: &str,
        file: &mut File,
        size: u64,
        digest: &str,
    ) -> Result<()> {
        if self.matches(key, digest, size, IMMUTABLE)? {
            self.reused += 1;
            return Ok(());
        }
        let response = self.request(
            "PUT",
            key,
            digest,
            &[
                ("if-none-match", "*"),
                ("cache-control", IMMUTABLE),
                ("content-type", "application/octet-stream"),
                ("x-amz-meta-sha256", digest),
            ],
            Some((file, size)),
        )?;
        match response.status().as_u16() {
            200 => {
                self.uploaded += 1;
                Ok(())
            }
            409 | 412 if self.matches(key, digest, size, IMMUTABLE)? => {
                self.reused += 1;
                Ok(())
            }
            code => Err(format!(
                "S3 PUT {key}: HTTP {code}; immutable object not replaced"
            )),
        }
    }
    pub(crate) fn head(
        &self,
        key: &str,
        file: &mut File,
        bytes: &[u8],
        previous: Option<&Remote>,
    ) -> Result<()> {
        let condition = previous
            .map(|r| ("if-match", r.etag.as_str()))
            .unwrap_or(("if-none-match", "*"));
        let response = self.request(
            "PUT",
            key,
            &hash(bytes),
            &[
                condition,
                ("cache-control", NO_STORE),
                ("content-type", "application/octet-stream"),
            ],
            Some((file, bytes.len() as u64)),
        )?;
        match response.status().as_u16() {
            200 => Ok(()),
            409 | 412 if self.get(key, 16384)?.is_some_and(|r| r.bytes == bytes) => Ok(()),
            code => Err(format!(
                "S3 head publication refused (HTTP {code}); rerun after checking the remote head"
            )),
        }
    }
}
fn header<'a>(
    response: &'a ureq::http::Response<ureq::Body>,
    name: &str,
) -> Result<Option<&'a str>> {
    let mut values = response.headers().get_all(name).iter();
    let value = values
        .next()
        .map(|v| {
            v.to_str()
                .map_err(|_| "invalid S3 response header".to_string())
        })
        .transpose()?;
    if values.next().is_some() {
        return Err("duplicate S3 response header".into());
    }
    Ok(value)
}

pub(crate) fn file_hash(file: &mut File, size: u64, client: &Client) -> Result<String> {
    file_hash_until(file, size, client.deadline())
}

pub(crate) fn file_hash_until(file: &mut File, size: u64, deadline: Instant) -> Result<String> {
    if size > MAX_FILE {
        return Err("file exceeds the four-GiB hashing/upload bound".into());
    }
    io(file.seek(SeekFrom::Start(0)))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 65536];
    let mut total = 0;
    loop {
        if Instant::now() >= deadline {
            return Err("file hashing exceeded its deadline".into());
        }
        let count = io(file.read(&mut buffer))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > size {
            return Err("S3 source changed length while hashing".into());
        }
        digest.update(buffer.get(..count).ok_or("S3 read length")?);
    }
    if total != size {
        return Err("S3 source changed length while hashing".into());
    }
    Ok(sig::to_hex(&digest.finalize()))
}

const HELP: &str = "td-net s3 put FILE KEY ENDPOINT REGION BUCKET CREDENTIALS PROFILE [--dry-run]";
fn command(args: &[String]) -> Result<()> {
    let words: Vec<_> = args.iter().skip(1).map(String::as_str).collect();
    let (words, dry) = if words.last() == Some(&"--dry-run") {
        (words.get(..words.len() - 1).ok_or(HELP)?, true)
    } else {
        (words.as_slice(), false)
    };
    let ["put", path, key, endpoint, region, bucket, credentials, profile] = words else {
        return Err(HELP.into());
    };
    if matches!(
        key.split('/').next(),
        Some("heads" | "roots" | "releases" | "objects")
    ) {
        return Err("use release upload-s3 for reserved release keys".into());
    }
    key_path(bucket, key)?;
    let mut client = Client::new(endpoint, region, bucket, Path::new(credentials), profile)?;
    let (mut file, meta) = io(td_fs::open_real_file(Path::new(path), "S3 upload file"))?;
    let digest = file_hash(&mut file, meta.len(), &client)?;
    println!(
        "S3 {}: {key}, {} bytes, SHA-256 {digest}",
        if dry { "dry run" } else { "upload" },
        meta.len()
    );
    if !dry {
        client.immutable(key, &mut file, meta.len(), &digest)?;
    }
    Ok(())
}
pub(crate) fn run(args: &[String]) {
    if let Err(error) = command(args) {
        eprintln!("td-net s3: {error}");
        std::process::exit(1);
    }
}
