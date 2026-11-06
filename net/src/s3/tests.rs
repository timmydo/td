#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread::JoinHandle;

const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
fn credentials() -> Credentials {
    Credentials {
        access: ACCESS.into(),
        secret: SECRET.into(),
        token: None,
    }
}
pub(crate) fn write_credentials(path: &Path) {
    fs::write(
        path,
        format!("[test]\naws_access_key_id = {ACCESS}\naws_secret_access_key = {SECRET}\n"),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
#[test]
fn aws_published_get_and_put_signatures() {
    // AWS S3 single-chunk SigV4 examples, 2013-05-24; two independent answers.
    let stamp = "20130524T000000Z";
    let empty = hash(b"");
    let mut headers = BTreeMap::from([
        ("host".into(), "examplebucket.s3.amazonaws.com".into()),
        ("range".into(), "bytes=0-9".into()),
        ("x-amz-content-sha256".into(), empty.clone()),
        ("x-amz-date".into(), stamp.into()),
    ]);
    let auth = authorization(
        &credentials(),
        "GET",
        "/test.txt",
        "us-east-1",
        stamp,
        &headers,
        &empty,
    )
    .unwrap();
    assert!(auth
        .ends_with("Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"));
    headers.remove("range");
    headers.insert("date".into(), "Fri, 24 May 2013 00:00:00 GMT".into());
    headers.insert("x-amz-storage-class".into(), "REDUCED_REDUNDANCY".into());
    let digest = hash(b"Welcome to Amazon S3.");
    headers.insert("x-amz-content-sha256".into(), digest.clone());
    let auth = authorization(
        &credentials(),
        "PUT",
        "/test%24file.text",
        "us-east-1",
        stamp,
        &headers,
        &digest,
    )
    .unwrap();
    assert!(auth
        .ends_with("Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"));
}
#[test]
fn key_encoding_dates_and_credentials_are_bounded() {
    assert_eq!(
        key_path("td-releases", "iso/td $é.iso").unwrap(),
        "/td-releases/iso/td%20%24%C3%A9.iso"
    );
    for key in ["", "/a", "a//b", "a/../b", "./x", "a\\b", "a\nb"] {
        assert!(key_path("bucket", key).is_err());
    }
    assert_eq!(timestamp(0).unwrap(), "19700101T000000Z");
    assert_eq!(timestamp(1369353600).unwrap(), "20130524T000000Z");
    assert_eq!(timestamp(951782400).unwrap(), "20000229T000000Z");
    assert!(timestamp(u64::MAX).is_err());
    for text in [
        "[test]\naws_access_key_id=A\n",
        "[test]\naws_access_key_id=A\naws_secret_access_key=x\n[test]\n",
        "[test]\naws_access_key_id=A\naws_secret_access_key=x\naws_secret_access_key=y",
        "[test]\ncredential_process=anything",
    ] {
        assert!(Credentials::parse(text, "test").is_err());
    }
    let c = Credentials::parse("[unused]\nanything=x\n[test]\naws_access_key_id=A\naws_secret_access_key=x\naws_session_token=token+/=", "test").unwrap();
    assert_eq!(c.token.as_deref(), Some("token+/="));
    let dir = crate::test_temp::TempDir::new("s3-permissions").unwrap();
    let path = dir.join("credentials");
    write_credentials(&path);
    assert!(Credentials::load(&path, "test").is_ok());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Credentials::load(&path, "test").is_err());
    let link = dir.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(Credentials::load(&link, "test").is_err());
}

#[derive(Clone)]
pub(crate) struct Stored {
    pub(crate) bytes: Vec<u8>,
    pub(crate) headers: BTreeMap<String, String>,
}
#[derive(Default)]
pub(crate) struct Store {
    pub(crate) files: BTreeMap<String, Stored>,
    pub(crate) requests: Vec<(String, String)>,
    pub(crate) fail: Option<(String, u16, usize)>,
    pub(crate) lose_reply: Option<String>,
    pub(crate) race_head: Option<Vec<u8>>,
    pub(crate) age_on_delete: Option<std::path::PathBuf>,
    race_file: Option<(String, Stored)>,
    fail_put: Option<(String, u16)>,
}
pub(crate) struct Mock {
    pub(crate) endpoint: String,
    pub(crate) store: Arc<Mutex<Store>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Mock {
    pub(crate) fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let store = Arc::new(Mutex::new(Store::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, quit) = (store.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            while !quit.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &s),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        Self {
            endpoint,
            store,
            stop,
            worker: Some(worker),
        }
    }
    pub(crate) fn client(&self, path: &Path) -> Client {
        Client::new(&self.endpoint, "auto", "td-releases", path, "test").unwrap()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }
}
fn serve(mut stream: TcpStream, shared: &Mutex<Store>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut first = String::new();
    reader.read_line(&mut first).unwrap();
    if first.is_empty() {
        return;
    }
    let words: Vec<_> = first.split_whitespace().collect();
    let method = words[0];
    let (path, query) = words[1].split_once('?').unwrap_or((words[1], ""));
    let key = path.strip_prefix("/td-releases/").unwrap().to_string();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        let (k, v) = line.trim_end().split_once(':').unwrap();
        headers.insert(k.to_ascii_lowercase(), v.trim().to_string());
        assert!(headers.len() < 40);
    }
    let length: usize = headers
        .get("content-length")
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    assert!(length <= 16 * 1024 * 1024);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    assert_eq!(headers["x-amz-content-sha256"], hash(&body));
    let signed = headers["authorization"]
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let canonical: BTreeMap<_, _> = signed
        .split(';')
        .map(|k| (k.to_string(), headers[k].clone()))
        .collect();
    assert_eq!(
        headers["authorization"],
        authorization_query(
            &credentials(),
            method,
            path,
            query,
            "auto",
            &headers["x-amz-date"],
            &canonical,
            &hash(&body)
        )
        .unwrap()
    );
    let mut store = shared.lock().unwrap();
    store.requests.push((method.into(), key.clone()));
    if let Some((target, status, count)) = &mut store.fail {
        if target == &key && *count > 0 {
            *count -= 1;
            reply(&mut stream, *status, &[], &[], method);
            return;
        }
    }
    if !query.is_empty() {
        assert_eq!(method, "GET");
        assert!(key.is_empty());
        assert!(query.ends_with("list-type=2&max-keys=1000&prefix=releases%2F"));
        let keys: Vec<_> = store
            .files
            .keys()
            .filter(|k| k.starts_with("releases/"))
            .collect();
        let start: usize = query
            .strip_prefix("continuation-token=page")
            .map(|s| s.split('&').next().unwrap().parse().unwrap())
            .unwrap_or(0);
        let part: Vec<_> = keys.iter().skip(start).take(1).collect();
        let more = start + part.len() < keys.len();
        let mut xml = format!(
            "<ListBucketResult><KeyCount>{}</KeyCount><IsTruncated>{more}</IsTruncated>",
            part.len()
        );
        for k in part {
            xml.push_str(&format!("<Contents><Key>{k}</Key></Contents>"));
        }
        if more {
            xml.push_str(&format!(
                "<NextContinuationToken>page{}</NextContinuationToken>",
                start + 1
            ));
        }
        xml.push_str("</ListBucketResult>");
        reply(&mut stream, 200, &[], xml.as_bytes(), method);
        return;
    }
    match method {
        "DELETE" => {
            if let Some(path) = store.age_on_delete.take() {
                File::open(path)
                    .unwrap()
                    .set_times(
                        std::fs::FileTimes::new()
                            .set_modified(SystemTime::now() - Duration::from_secs(9 * 86400)),
                    )
                    .unwrap();
            }
            if store.files.get(&key).is_some_and(|f| {
                headers.get("if-match") != Some(&format!("\"{}\"", hash(&f.bytes)))
            }) {
                reply(&mut stream, 412, &[], &[], method);
                return;
            }
            store.files.remove(&key);
            reply(&mut stream, 204, &[], &[], method);
        }
        "PUT" => {
            if let Some((target, status)) = &store.fail_put {
                if target == &key {
                    reply(&mut stream, *status, &[], &[], method);
                    return;
                }
            }
            if key.starts_with("heads/") || key == "index.html" {
                if let Some(bytes) = store.race_head.take() {
                    store.files.insert(
                        key.clone(),
                        Stored {
                            bytes,
                            headers: BTreeMap::new(),
                        },
                    );
                }
            }
            if store
                .race_file
                .as_ref()
                .is_some_and(|(target, _)| target == &key)
            {
                let (target, file) = store.race_file.take().unwrap();
                store.files.insert(target, file);
            }
            let existing = store.files.get(&key);
            if headers.get("if-none-match").is_some_and(|s| s == "*") && existing.is_some()
                || headers.get("if-match").is_some_and(|s| {
                    existing.is_none_or(|e| s != &format!("\"{}\"", hash(&e.bytes)))
                })
            {
                reply(&mut stream, 412, &[], &[], method);
                return;
            }
            assert!(headers.contains_key("if-none-match") || headers.contains_key("if-match"));
            store.files.insert(
                key.clone(),
                Stored {
                    bytes: body,
                    headers,
                },
            );
            if store.lose_reply.as_ref() == Some(&key) {
                store.lose_reply = None;
                return;
            }
            reply(&mut stream, 200, &[], &[], method);
        }
        "GET" | "HEAD" => {
            if let Some(file) = store.files.get(&key) {
                let mut extra = vec![("etag".to_string(), format!("\"{}\"", hash(&file.bytes)))];
                for name in ["cache-control", "x-amz-meta-sha256", "content-type"] {
                    if let Some(v) = file.headers.get(name) {
                        extra.push((name.into(), v.clone()));
                    }
                }
                reply(&mut stream, 200, &extra, &file.bytes, method);
            } else {
                reply(&mut stream, 404, &[], &[], method);
            }
        }
        _ => panic!("unexpected method {method}"),
    }
}
fn reply(
    stream: &mut TcpStream,
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    method: &str,
) {
    write!(
        stream,
        "HTTP/1.1 {status} Test\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    )
    .unwrap();
    for (k, v) in headers {
        write!(stream, "{k}: {v}\r\n").unwrap();
    }
    write!(stream, "\r\n").unwrap();
    if method != "HEAD" {
        stream.write_all(body).unwrap();
    }
}

#[test]
fn immutable_upload_reuses_retries_and_refuses_conflicts() {
    let dir = crate::test_temp::TempDir::new("s3-upload").unwrap();
    let cred = dir.join("credentials");
    write_credentials(&cred);
    let path = dir.join("image");
    let bytes = vec![42; 200_000];
    fs::write(&path, &bytes).unwrap();
    let mock = Mock::new();
    let mut client = mock.client(&cred);
    let mut file = File::open(path).unwrap();
    mock.store.lock().unwrap().lose_reply = Some("iso/test.iso".into());
    client
        .immutable("iso/test.iso", &mut file, bytes.len() as u64, &hash(&bytes))
        .unwrap();
    client
        .immutable("iso/test.iso", &mut file, bytes.len() as u64, &hash(&bytes))
        .unwrap();
    assert!(client
        .immutable(
            "iso/test.iso",
            &mut file,
            bytes.len() as u64,
            &hash(b"other")
        )
        .is_err());
    mock.store.lock().unwrap().fail = Some(("iso/retry.iso".into(), 503, 2));
    client
        .immutable(
            "iso/retry.iso",
            &mut file,
            bytes.len() as u64,
            &hash(&bytes),
        )
        .unwrap();
    mock.store.lock().unwrap().fail = Some(("iso/denied.iso".into(), 403, 99));
    assert!(client
        .immutable(
            "iso/denied.iso",
            &mut file,
            bytes.len() as u64,
            &hash(&bytes)
        )
        .is_err());
    assert_eq!(
        mock.store
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(_, k)| k == "iso/denied.iso")
            .count(),
        1
    );
    mock.store.lock().unwrap().fail = Some(("iso/redirect.iso".into(), 307, 99));
    assert!(client
        .immutable(
            "iso/redirect.iso",
            &mut file,
            bytes.len() as u64,
            &hash(&bytes)
        )
        .is_err());
    client.deadline = Instant::now();
    assert!(client
        .immutable(
            "iso/deadline.iso",
            &mut file,
            bytes.len() as u64,
            &hash(&bytes)
        )
        .is_err());
}

#[test]
fn single_file_dry_run_sends_nothing_and_reserved_names_refuse() {
    let dir = crate::test_temp::TempDir::new("s3-dry").unwrap();
    let cred = dir.join("credentials");
    write_credentials(&cred);
    let path = dir.join("file");
    fs::write(&path, b"ISO fixture").unwrap();
    let mock = Mock::new();
    let args = vec![
        "s3".into(),
        "put".into(),
        path.to_str().unwrap().into(),
        "iso/test.iso".into(),
        mock.endpoint.clone(),
        "auto".into(),
        "td-releases".into(),
        cred.to_str().unwrap().into(),
        "test".into(),
        "--dry-run".into(),
    ];
    command(&args).unwrap();
    assert!(mock.store.lock().unwrap().requests.is_empty());
    let mut reserved = args;
    reserved[3] = "heads/system-x86-64".into();
    assert!(command(&reserved).is_err());
    for endpoint in [
        "http://example.org",
        "https://user@example.org",
        "https://example.org/path",
        "https://example.org?x",
        "https://example.org/#x",
    ] {
        assert!(Client::new(endpoint, "auto", "td-releases", &cred, "test").is_err());
    }
}

fn website_files(dir: &Path) {
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::write(dir.join("style.css"), b"body { color: black; }").unwrap();
    fs::write(dir.join("assets/favicon.svg"), b"<svg/>").unwrap();
    fs::write(dir.join("assets/td-desktop.jpg"), b"JPEG fixture").unwrap();
    fs::write(dir.join("index.html"), b"<!doctype html><title>td</title>").unwrap();
    fs::write(dir.join("README.md"), b"not public").unwrap();
}

#[test]
fn website_upload_orders_types_reuses_and_replaces() {
    let dir = crate::test_temp::TempDir::new("s3-website").unwrap();
    website_files(&dir);
    let cred = dir.join("credentials");
    write_credentials(&cred);
    let mock = Mock::new();
    let args: Vec<String> = [
        "s3",
        "website",
        dir.to_str().unwrap(),
        &mock.endpoint,
        "auto",
        "td-releases",
        cred.to_str().unwrap(),
        "test",
        "--dry-run",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    command(&args).unwrap();
    assert!(mock.store.lock().unwrap().requests.is_empty());
    let mut client = mock.client(&cred);
    mock.store.lock().unwrap().lose_reply = Some("style.css".into());
    website::upload(&mut client, &dir, &cred, false).unwrap();
    {
        let store = mock.store.lock().unwrap();
        assert_eq!(store.files.len(), 4);
        assert_eq!(
            store.requests.iter().rfind(|(m, _)| m == "PUT").unwrap().1,
            "index.html"
        );
        for (key, mime) in [
            ("index.html", "text/html; charset=utf-8"),
            ("style.css", "text/css; charset=utf-8"),
            ("assets/favicon.svg", "image/svg+xml"),
            ("assets/td-desktop.jpg", "image/jpeg"),
        ] {
            assert_eq!(store.files[key].headers["content-type"], mime);
            assert_eq!(store.files[key].headers["cache-control"], "no-cache");
        }
    }
    mock.store.lock().unwrap().requests.clear();
    website::upload(&mut client, &dir, &cred, false).unwrap();
    assert!(mock
        .store
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|(m, _)| m == "HEAD"));
    fs::write(dir.join("index.html"), b"new page").unwrap();
    website::upload(&mut client, &dir, &cred, false).unwrap();
    assert_eq!(
        mock.store.lock().unwrap().files["index.html"].bytes,
        b"new page"
    );
    // Repair wrong HTTP metadata even when bytes are identical.
    mock.store
        .lock()
        .unwrap()
        .files
        .get_mut("style.css")
        .unwrap()
        .headers
        .insert("content-type".into(), "application/octet-stream".into());
    website::upload(&mut client, &dir, &cred, false).unwrap();
    assert_eq!(
        mock.store.lock().unwrap().files["style.css"].headers["content-type"],
        "text/css; charset=utf-8"
    );
}

#[test]
fn website_refuses_invalid_local_files_before_network_and_stops_on_asset_failure() {
    let dir = crate::test_temp::TempDir::new("s3-website-refuse").unwrap();
    website_files(&dir);
    let cred = dir.join("credentials");
    write_credentials(&cred);
    let mock = Mock::new();
    let mut client = mock.client(&cred);
    fs::remove_file(dir.join("index.html")).unwrap();
    assert!(website::upload(&mut client, &dir, &cred, false).is_err());
    assert!(mock.store.lock().unwrap().requests.is_empty());
    std::os::unix::fs::symlink(&cred, dir.join("index.html")).unwrap();
    assert!(website::upload(&mut client, &dir, &cred, true).is_err());
    fs::remove_file(dir.join("index.html")).unwrap();
    fs::hard_link(&cred, dir.join("index.html")).unwrap();
    assert!(website::upload(&mut client, &dir, &cred, true).is_err());
    fs::remove_file(dir.join("index.html")).unwrap();
    website_files(&dir);
    mock.store.lock().unwrap().fail_put = Some(("assets/favicon.svg".into(), 403));
    assert!(website::upload(&mut client, &dir, &cred, false).is_err());
    let store = mock.store.lock().unwrap();
    assert!(store.files.contains_key("style.css"));
    assert!(!store
        .requests
        .iter()
        .any(|(m, k)| m == "PUT" && k == "index.html"));
}

#[test]
fn website_conflicting_page_write_is_not_overwritten() {
    let dir = crate::test_temp::TempDir::new("s3-website-race").unwrap();
    website_files(&dir);
    let cred = dir.join("credentials");
    write_credentials(&cred);
    let mock = Mock::new();
    let mut client = mock.client(&cred);
    website::upload(&mut client, &dir, &cred, false).unwrap();
    fs::write(dir.join("index.html"), b"our next page").unwrap();
    mock.store.lock().unwrap().race_file = Some((
        "index.html".into(),
        Stored {
            bytes: b"concurrent page".to_vec(),
            headers: BTreeMap::new(),
        },
    ));
    assert!(website::upload(&mut client, &dir, &cred, false).is_err());
    assert_eq!(
        mock.store.lock().unwrap().files["index.html"].bytes,
        b"concurrent page"
    );
}
