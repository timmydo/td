use super::*;

const CACHE: &str = "no-cache";
const LIMIT: u64 = 8 * 1024 * 1024;
// This is an explicit public-file list, not a recursive directory upload.
const FILES: &[(&str, &str)] = &[
    ("style.css", "text/css; charset=utf-8"),
    ("assets/favicon.svg", "image/svg+xml"),
    ("assets/td-desktop.jpg", "image/jpeg"),
    ("index.html", "text/html; charset=utf-8"),
];

struct Asset {
    key: &'static str,
    mime: &'static str,
    file: File,
    size: u64,
    digest: String,
}
struct State {
    etag: String,
    size: Option<u64>,
    digest: Option<String>,
    cache: Option<String>,
    mime: Option<String>,
}
impl State {
    fn matches(&self, asset: &Asset) -> bool {
        self.size == Some(asset.size)
            && self.digest.as_deref() == Some(asset.digest.as_str())
            && self.cache.as_deref() == Some(CACHE)
            && self.mime.as_deref() == Some(asset.mime)
    }
}

fn state(client: &Client, key: &str) -> Result<Option<State>> {
    let response = client.request("HEAD", key, &hash(b""), &[], None)?;
    match response.status().as_u16() {
        404 => Ok(None),
        200 => Ok(Some(State {
            etag: etag(&response)?,
            size: header(&response, "content-length")?.and_then(|s| s.parse().ok()),
            digest: header(&response, "x-amz-meta-sha256")?.map(str::to_string),
            cache: header(&response, "cache-control")?.map(str::to_string),
            mime: header(&response, "content-type")?.map(str::to_string),
        })),
        status => Err(format!("S3 website HEAD {key}: HTTP {status}")),
    }
}

fn put(client: &mut Client, asset: &mut Asset, previous: Option<&State>) -> Result<()> {
    if previous.is_some_and(|s| s.matches(asset)) {
        client.reused += 1;
        return Ok(());
    }
    let condition = previous
        .map(|s| ("if-match", s.etag.as_str()))
        .unwrap_or(("if-none-match", "*"));
    let response = client.request(
        "PUT",
        asset.key,
        &asset.digest,
        &[
            condition,
            ("content-type", asset.mime),
            ("cache-control", CACHE),
            ("x-amz-meta-sha256", &asset.digest),
        ],
        Some((&mut asset.file, asset.size)),
    )?;
    match response.status().as_u16() {
        200 => {
            client.uploaded += 1;
            Ok(())
        }
        409 | 412 if state(client, asset.key)?.is_some_and(|s| s.matches(asset)) => {
            client.reused += 1;
            Ok(())
        }
        status => Err(format!(
            "S3 website PUT {}: HTTP {status}; rerun after checking the remote site",
            asset.key
        )),
    }
}

pub(super) fn upload(
    client: &mut Client,
    directory: &Path,
    credentials: &Path,
    dry: bool,
) -> Result<()> {
    let credentials = io(fs::metadata(credentials))?;
    let mut assets = Vec::with_capacity(FILES.len());
    for &(key, mime) in FILES {
        let (mut file, meta) = io(td_fs::open_real_file(&directory.join(key), "website file"))?;
        if meta.dev() == credentials.dev() && meta.ino() == credentials.ino() {
            return Err("website public file is the credentials file".into());
        }
        if meta.len() == 0 || meta.len() > LIMIT {
            return Err(format!(
                "website file {key} must be nonempty and at most 8 MiB"
            ));
        }
        let digest = file_hash(&mut file, meta.len(), client)?;
        assets.push(Asset {
            key,
            mime,
            file,
            size: meta.len(),
            digest,
        });
    }
    for asset in &assets {
        println!(
            "S3 website {}: {}, {} bytes, {}",
            if dry { "dry run" } else { "planned" },
            asset.key,
            asset.size,
            asset.mime
        );
    }
    if dry {
        return Ok(());
    }
    // Snapshot every condition before writing; publish the entry page last.
    let previous = assets
        .iter()
        .map(|asset| state(client, asset.key))
        .collect::<Result<Vec<_>>>()?;
    for (asset, previous) in assets.iter_mut().zip(&previous) {
        put(client, asset, previous.as_ref())?;
    }
    println!(
        "S3 website: {} uploaded, {} reused; index.html published last",
        client.uploaded, client.reused
    );
    Ok(())
}
