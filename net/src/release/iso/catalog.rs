//! The bucket's ISO catalog: which signed installers the download page
//! lists, in publication order, and which await deletion. Every publisher
//! continues this one listing; no host keeps its own.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

/// The catalog's key; `no-store`, replaced only on its observed ETag.
pub(in crate::release) const KEY: &str = "publisher/iso-catalog";
/// Each listed release's source-digest record, keyed by its SHA-256.
const DIGESTS: &str = "publisher/source-digests";
const HEADER: &str = "td-iso-catalog-v2";
const LIMIT: u64 = 1024 * 1024;
/// Listed rows are bounded by the page (`index::MAX_ENTRIES`); retired rows
/// wait only until the next retirement that reaches them.
const MAX_ROWS: usize = 1024;
const DIGESTS_HEADER: &str = "td-source-digests-v1";
const MAX_DIGEST_RECORD: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    listed: bool,
    root: u64,
    profile: String,
    /// The signature record's SHA-256: `iso/signatures/SIGNATURE.sig`.
    signature: String,
    /// The source-digest record's SHA-256; none for a release published
    /// before `./publish` recorded one.
    digests: Option<String>,
}

impl Row {
    fn parse(line: &str) -> Result<Self> {
        let fields: Vec<&str> = line.split('\t').collect();
        let [kind, root, profile, signature, digests] = fields.as_slice() else {
            return Err("ISO catalog row needs five tab-separated fields".into());
        };
        let listed = match *kind {
            "listed" => true,
            "retired" => false,
            _ => return Err("ISO catalog row is neither listed nor retired".into()),
        };
        let root = format::number(root)?;
        if root == 0 {
            return Err("ISO catalog row names root epoch zero".into());
        }
        format::profile(profile)?;
        format::hex(signature, 32)?;
        let digests = match *digests {
            "-" => None,
            digest => {
                format::hex(digest, 32)?;
                Some(digest.to_string())
            }
        };
        Ok(Self {
            listed,
            root,
            profile: profile.to_string(),
            signature: signature.to_string(),
            digests,
        })
    }
    fn line(&self) -> String {
        format!(
            "{}\t{}\t{}\t{}\t{}\n",
            if self.listed { "listed" } else { "retired" },
            self.root,
            self.profile,
            self.signature,
            self.digests.as_deref().unwrap_or("-")
        )
    }
    fn signature_key(&self) -> String {
        format!("iso/signatures/{}.sig", self.signature)
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Catalog {
    rows: Vec<Row>,
}

impl Catalog {
    fn parse(text: &str) -> Result<Self> {
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) || !text.ends_with('\n') {
            return Err(format!("the bucket's ISO catalog is not {HEADER}"));
        }
        let mut catalog = Self { rows: Vec::new() };
        for line in lines {
            catalog.push(Row::parse(line)?)?;
        }
        Ok(catalog)
    }
    fn push(&mut self, row: Row) -> Result<()> {
        if self.rows.len() >= MAX_ROWS {
            return Err("the ISO catalog exceeds its row bound".into());
        }
        if self
            .rows
            .iter()
            .any(|other| other.signature == row.signature)
        {
            return Err("the ISO catalog names one signature twice".into());
        }
        self.rows.push(row);
        Ok(())
    }
    fn text(&self) -> String {
        let mut text = format!("{HEADER}\n");
        for row in &self.rows {
            text.push_str(&row.line());
        }
        text
    }
    fn listed(&self) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(|row| row.listed)
    }
}

/// List `row` after the listed rows, unless a retry already listed it, then
/// keep the newest `keep` listed rows of each profile; older ones become
/// retired, for `retire` to delete.
fn admit(catalog: &mut Catalog, row: Row, keep: usize) -> Result<()> {
    let listed = catalog
        .rows
        .iter()
        .any(|other| other.listed && other.signature == row.signature);
    if !listed {
        // A retired copy listed again goes back to the end.
        catalog
            .rows
            .retain(|other| other.signature != row.signature);
        catalog.push(row.clone())?;
    }
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for entry in catalog.rows.iter_mut().rev().filter(|r| r.listed) {
        let count = seen.entry(entry.profile.clone()).or_insert(0);
        if *count < keep {
            *count += 1;
        } else {
            entry.listed = false;
        }
    }
    if !catalog
        .rows
        .iter()
        .any(|r| r.listed && r.signature == row.signature)
    {
        return Err("the release being published is not among the newest kept".into());
    }
    Ok(())
}

/// A source-digest record: its header, its count, then one digest per line,
/// so a truncated copy refuses rather than reading as fewer pins.
fn parse_digests(text: &str) -> Result<Vec<&str>> {
    let mut lines = text.lines();
    if lines.next() != Some(DIGESTS_HEADER) {
        return Err("invalid source digest record header".into());
    }
    let count: usize = lines
        .next()
        .and_then(|line| line.parse().ok())
        .ok_or("invalid source digest record count")?;
    let digests: Vec<&str> = lines.collect();
    if count == 0 || digests.len() != count || !digests.iter().all(|d| format::hex(d, 32).is_ok()) {
        return Err("source digest record is empty, truncated or malformed".into());
    }
    Ok(digests)
}

/// A local digest record, checked, with its open file to upload.
fn read_digests(path: &Path) -> Result<(File, String)> {
    let (mut file, meta) = io(td_fs::open_real_file(path, "source digest record"))?;
    let mut value = String::new();
    io(Read::by_ref(&mut file)
        .take(MAX_DIGEST_RECORD + 1)
        .read_to_string(&mut value))?;
    if meta.len() > MAX_DIGEST_RECORD || value.len() as u64 != meta.len() {
        return Err(format!(
            "{}: source digest record too large or changed",
            path.display()
        ));
    }
    parse_digests(&value).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((file, value))
}

/// The trusted root chain and a client for `[ENDPOINT REGION BUCKET
/// CREDENTIALS PROFILE]`.
fn open(root: &Path, words: &[&str]) -> Result<(s3::Client, Vec<Root>)> {
    let [endpoint, region, bucket, credentials, credential_profile] = words else {
        return Err(HELP.into());
    };
    let trusted = Root::parse(&text(root, SMALL)?, None)?;
    trusted.fresh(now()?)?;
    let client = s3::Client::new(
        endpoint,
        region,
        bucket,
        Path::new(credentials),
        credential_profile,
    )?;
    let chain = pull::chain(&trusted, &client, |_| Ok(None))?.0;
    Ok((client, chain))
}

fn parse_remote(remote: &s3::Remote) -> Result<Catalog> {
    Catalog::parse(std::str::from_utf8(&remote.bytes).map_err(|_| "ISO catalog UTF-8")?)
}

fn fetch(client: &s3::Client) -> Result<(s3::Remote, Catalog)> {
    let remote = client.get(KEY, LIMIT)?.ok_or(
        "the bucket holds no ISO catalog; ./publish imports the local one (net/OPERATIONS.md)",
    )?;
    let catalog = parse_remote(&remote)?;
    Ok((remote, catalog))
}

/// Refuse when the bucket's catalog is no longer `expected`: another host
/// wrote since, and what this run decided from it may be stale.
fn current(client: &s3::Client, expected: &[u8]) -> Result<()> {
    if client.get(KEY, LIMIT)?.is_some_and(|r| r.bytes == expected) {
        Ok(())
    } else {
        Err("the ISO catalog changed since this run read it; rerun".into())
    }
}

/// A row's signature record from the bucket, verified under its root now;
/// `None` once retirement deleted it.
fn record(client: &s3::Client, chain: &[Root], row: &Row) -> Result<Option<(Record, String)>> {
    let Some(remote) = client.get(&row.signature_key(), SMALL)? else {
        return Ok(None);
    };
    if hash(&remote.bytes) != row.signature {
        return Err(format!(
            "remote {} does not match its digest",
            row.signature_key()
        ));
    }
    let value = String::from_utf8(remote.bytes).map_err(|_| "ISO signature is not UTF-8")?;
    let root = pull::epoch(chain, row.root)?;
    Ok(Some((
        Record::parse(&value, root, &row.profile, now()?)?,
        value,
    )))
}

/// Whether the bucket holds `record`'s image with its length and SHA-256.
fn present(client: &s3::Client, record: &Record) -> Result<bool> {
    client.matches(
        &record.image_key(),
        &record.digest,
        record.size,
        s3::IMMUTABLE,
    )
}

/// The page's entries: every listed row's record, and its image present in
/// the bucket with the record's length and SHA-256.
fn entries(client: &s3::Client, chain: &[Root], catalog: &Catalog) -> Result<Vec<index::Entry>> {
    let mut entries: Vec<index::Entry> = Vec::new();
    for row in catalog.listed() {
        client.remaining()?;
        let (record, signature) = record(client, chain, row)?
            .ok_or_else(|| format!("listed {} is missing from the bucket", row.signature_key()))?;
        if !present(client, &record)? {
            return Err(format!(
                "listed {} is missing from the bucket",
                record.image_key()
            ));
        }
        entries.push(index::Entry {
            profile: row.profile.clone(),
            root: row.root,
            record,
            signature,
        });
    }
    Ok(entries)
}

/// The page for `entries`, refusing one image listed twice for a profile.
fn page(entries: &mut [index::Entry]) -> Result<String> {
    for (position, entry) in entries.iter().enumerate() {
        let image = entry.record.image_key();
        if entries
            .iter()
            .skip(position + 1)
            .any(|e| e.profile == entry.profile && e.record.image_key() == image)
        {
            return Err(format!(
                "the ISO catalog would list {image} twice for {}; unlist the older row (unlist-iso-s3)",
                entry.profile
            ));
        }
    }
    index::render(entries)
}

/// `bytes` written to `path`, reopened and checked, to send as a body.
fn staged(path: &Path, bytes: &[u8]) -> Result<File> {
    replace(path, bytes)?;
    let (mut file, meta) = io(td_fs::open_real_file(path, "staged ISO catalog file"))?;
    if meta.len() != bytes.len() as u64
        || s3::file_hash_until(
            &mut file,
            meta.len(),
            Instant::now() + Duration::from_secs(60),
        )? != hash(bytes)
    {
        return Err(format!("{} changed before upload", path.display()));
    }
    Ok(file)
}

/// Replace the bucket's catalog with `catalog`, on the ETag it was read at.
fn store(
    client: &s3::Client,
    output: &Path,
    catalog: &Catalog,
    previous: Option<&s3::Remote>,
) -> Result<()> {
    let text = catalog.text();
    if previous.is_some_and(|remote| remote.bytes == text.as_bytes()) {
        return Ok(());
    }
    let mut file = staged(&output.join("iso-catalog"), text.as_bytes())?;
    client.state(KEY, &mut file, text.as_bytes(), previous, LIMIT)
}

/// Replace the catalog, then the page rendered from it. The page's ETag is
/// read before the catalog is written, so a page another host rendered
/// from a later catalog refuses this one rather than being replaced by it.
fn write(
    client: &s3::Client,
    output: &Path,
    catalog: &Catalog,
    previous: &s3::Remote,
    page: &str,
) -> Result<()> {
    let page_before = client.get("index.html", LIMIT)?;
    store(client, output, catalog, Some(previous))?;
    let mut file = staged(&output.join("index.html"), page.as_bytes())?;
    client.html_index(&mut file, page.as_bytes(), page_before.as_ref())
}

/// `list-iso-s3 ROOT PROFILE SIGNATURE ISO DIGESTS RETAIN OUTPUT S3... [--check]`:
/// list one signed ISO in the bucket's catalog and republish the page.
pub(in crate::release) fn list(words: &[&str]) -> Result<()> {
    let (words, check) = match words.split_last() {
        Some((&"--check", rest)) => (rest, true),
        _ => (words, false),
    };
    let [root, profile, signature, image, digests, keep, output, s3_words @ ..] = words else {
        return Err(HELP.into());
    };
    let keep = format::number(keep)?;
    let keep = usize::try_from(keep)
        .ok()
        .filter(|keep| (1..=index::MAX_ENTRIES).contains(keep))
        .ok_or("RETAIN must be a whole number from 1 to 128")?;
    let output = Path::new(output);
    directory(output)?;
    let (mut client, chain) = open(Path::new(root), s3_words)?;
    let mut verified = verify(
        Path::new(root),
        profile,
        Path::new(signature),
        Path::new(image),
        client.deadline(),
    )?;
    let (mut digests_file, digests_text) = read_digests(Path::new(digests))?;
    let digests_hash = hash(digests_text.as_bytes());
    let signature_hash = hash(verified.signature.as_bytes());
    let (remote, mut catalog) = fetch(&client)?;
    admit(
        &mut catalog,
        Row {
            listed: true,
            root: verified.root.version,
            profile: profile.to_string(),
            signature: signature_hash.clone(),
            digests: Some(digests_hash.clone()),
        },
        keep,
    )?;
    if check {
        // This release's image may not be in the bucket yet: its entry is
        // the one just verified locally, every other one the bucket's.
        let mut others = Catalog { rows: Vec::new() };
        for row in catalog.listed() {
            if row.signature != signature_hash {
                others.push(row.clone())?;
            }
        }
        let mut entries = entries(&client, &chain, &others)?;
        entries.push(index::Entry {
            profile: profile.to_string(),
            root: verified.root.version,
            record: verified.record,
            signature: verified.signature,
        });
        page(&mut entries)?;
        println!(
            "ISO catalog check: {} listed after this release",
            entries.len()
        );
        return Ok(());
    }
    super::publish(&mut client, &mut verified, profile)?;
    client.immutable(
        &format!("{DIGESTS}/{digests_hash}"),
        &mut digests_file,
        digests_text.len() as u64,
        &digests_hash,
    )?;
    let mut entries = entries(&client, &chain, &catalog)?;
    let page = page(&mut entries)?;
    // The catalog first: a lost page is rebuilt from it by a rerun.
    write(&client, output, &catalog, &remote, &page)?;
    println!(
        "ISO catalog: {} listed, {} awaiting retirement; page published last",
        entries.len(),
        catalog.rows.len() - entries.len()
    );
    Ok(())
}

/// `unlist-iso-s3 ROOT OUTPUT S3... SIGNATURE_SHA256...`: retire the named
/// rows now, whether or not their records still verify, and republish the
/// page without them; `retire-s3` then deletes their files.
pub(in crate::release) fn unlist(words: &[&str]) -> Result<()> {
    let [root, output, endpoint, region, bucket, credentials, profile, signatures @ ..] = words
    else {
        return Err(HELP.into());
    };
    if signatures.is_empty() {
        return Err(HELP.into());
    }
    for signature in signatures {
        format::hex(signature, 32)?;
    }
    let output = Path::new(output);
    directory(output)?;
    let (client, chain) = open(
        Path::new(root),
        &[endpoint, region, bucket, credentials, profile],
    )?;
    let (remote, mut catalog) = fetch(&client)?;
    for signature in signatures {
        let row = catalog
            .rows
            .iter_mut()
            .find(|row| row.signature == *signature)
            .ok_or_else(|| format!("the ISO catalog has no row for {signature}"))?;
        row.listed = false;
    }
    let mut entries = entries(&client, &chain, &catalog)?;
    let page = page(&mut entries)?;
    write(&client, output, &catalog, &remote, &page)?;
    println!(
        "unlisted {} row(s); retire-s3 deletes their files",
        signatures.len()
    );
    Ok(())
}

/// `retire-s3 ROOT OUTPUT S3... [--delete --exclusive-publisher]`: delete
/// each retired row's image and signature (only unlisting one whose image a
/// listed row shares), then mirrored sources no listed release pins.
pub(in crate::release) fn retire(words: &[&str]) -> Result<()> {
    let (words, delete) = match words {
        [rest @ .., "--delete", "--exclusive-publisher"] => (rest, true),
        _ => (words, false),
    };
    let [root, output, s3_words @ ..] = words else {
        return Err(HELP.into());
    };
    let output = Path::new(output);
    directory(output)?;
    let (client, chain) = open(Path::new(root), s3_words)?;
    let (remote, catalog) = fetch(&client)?;
    let listed_images: BTreeSet<String> = entries(&client, &chain, &catalog)?
        .iter()
        .map(|entry| entry.record.image_key())
        .collect();
    let mut failures = Vec::new();
    let mut changed = false;
    let mut kept = Catalog { rows: Vec::new() };
    for row in &catalog.rows {
        if row.listed {
            kept.push(row.clone())?;
            continue;
        }
        client.remaining()?;
        let finished = match record(&client, &chain, row) {
            Err(e) => Err(e),
            // The image goes first, so a missing record means both are gone.
            Ok(None) => Ok(true),
            Ok(Some((record, _))) if listed_images.contains(&record.image_key()) => {
                println!(
                    "{} {} (its image stays listed)",
                    if delete { "unlisted" } else { "would-unlist" },
                    row.signature_key()
                );
                Ok(true)
            }
            Ok(Some((record, signature))) => {
                // Another host may have listed this row again since.
                if delete {
                    if let Err(e) = current(&client, &remote.bytes) {
                        failures.push(format!("retire {}: {e}", row.signature_key()));
                        changed = true;
                        break;
                    }
                }
                (|| {
                    for (key, digest, size) in [
                        (record.image_key(), record.digest.clone(), record.size),
                        (
                            row.signature_key(),
                            row.signature.clone(),
                            signature.len() as u64,
                        ),
                    ] {
                        let present = client.prune(&key, &["iso"], &digest, Some(size), delete)?;
                        println!(
                            "{} {key}",
                            match (present, delete) {
                                (false, _) => "absent",
                                (true, true) => "deleted",
                                (true, false) => "would-delete",
                            }
                        );
                    }
                    Ok(delete)
                })()
            }
        };
        match finished {
            Ok(true) if delete => {}
            Ok(_) => kept.push(row.clone())?,
            Err(e) => {
                failures.push(format!("retire {}: {e}", row.signature_key()));
                kept.push(row.clone())?;
            }
        }
    }
    let collect = if changed {
        Err("catalog write and source mirror cleanup skipped".to_string())
    } else if delete {
        // The keep set must be the bucket's listing, not a stale view of it.
        store(&client, output, &kept, Some(&remote))
            .and_then(|()| current(&client, kept.text().as_bytes()))
            .map_err(|e| format!("ISO catalog: {e}; source mirror cleanup skipped"))
    } else {
        Ok(())
    };
    match collect {
        Ok(()) => {
            if let Err(e) = mirror(&client, &kept, delete) {
                failures.push(format!("source mirror cleanup: {e}"));
            }
        }
        Err(e) => failures.push(e),
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "retention is incomplete; rerun, or see net/OPERATIONS.md, \"Storage retention\":\n{}",
            failures.join("\n")
        ))
    }
}

/// Delete every mirrored source no listed release's digest record pins.
fn mirror(client: &s3::Client, catalog: &Catalog, delete: bool) -> Result<()> {
    let mut keep = BTreeSet::new();
    for row in catalog.listed() {
        let Some(digests) = &row.digests else {
            println!(
                "source mirror retention waits until every listed release has its digest record ({} has none)",
                row.signature_key()
            );
            return Ok(());
        };
        client.remaining()?;
        let key = format!("{DIGESTS}/{digests}");
        let remote = client
            .get(&key, MAX_DIGEST_RECORD)?
            .ok_or_else(|| format!("{key} is missing from the bucket"))?;
        if hash(&remote.bytes) != *digests {
            return Err(format!("remote {key} does not match its digest"));
        }
        let value = std::str::from_utf8(&remote.bytes).map_err(|_| "digest record UTF-8")?;
        for digest in parse_digests(value).map_err(|e| format!("{key}: {e}"))? {
            keep.insert(digest.to_string());
        }
    }
    if keep.is_empty() {
        return Err("no listed release to keep the source mirror for".into());
    }
    let keep: BTreeSet<&str> = keep.iter().map(String::as_str).collect();
    s3::sources::collect_keep(&keep, client, delete)
}

/// A legacy row's files, relative to the legacy catalog's directory.
struct Legacy {
    row: Row,
    root: PathBuf,
    signature: PathBuf,
    image: PathBuf,
}

/// One `td-iso-catalog-v1` or `td-iso-retired-v1` row (ROOT, PROFILE,
/// SIGNATURE and ISO paths, relative to `base`), its record verified under
/// a root the bucket's chain holds.
fn legacy(chain: &[Root], base: &Path, line: &str, listed: bool) -> Result<Legacy> {
    let fields: Vec<&str> = line.split('\t').collect();
    let [root_path, profile, signature_path, image] = fields.as_slice() else {
        return Err("legacy catalog row needs ROOT, PROFILE, SIGNATURE, ISO".into());
    };
    let root_path = base.join(root_path);
    let root = Root::parse(&text(&root_path, SMALL)?, None)?;
    if pull::epoch(chain, root.version)?.text != root.text {
        return Err("a legacy catalog row's root is not the bucket's for its epoch".into());
    }
    let signature_path = base.join(signature_path);
    let signature = text(&signature_path, SMALL)?;
    Record::parse(&signature, &root, profile, now()?)?;
    Ok(Legacy {
        row: Row {
            listed,
            root: root.version,
            profile: profile.to_string(),
            signature: hash(signature.as_bytes()),
            digests: None,
        },
        root: root_path,
        signature: signature_path,
        image: base.join(image),
    })
}

/// The record `./publish` kept beside a legacy row's ISO, uploaded; `None`
/// when that release has none.
fn legacy_digests(client: &mut s3::Client, image: &Path) -> Result<Option<String>> {
    let path = image
        .parent()
        .ok_or("legacy ISO has no directory")?
        .join("source-digests");
    if !io(path.try_exists())? {
        return Ok(None);
    }
    let (mut file, value) = read_digests(&path)?;
    let digest = hash(value.as_bytes());
    client.immutable(
        &format!("{DIGESTS}/{digest}"),
        &mut file,
        value.len() as u64,
        &digest,
    )?;
    Ok(Some(digest))
}

/// The signatures a legacy catalog lists, by SHA-256; rows whose signature
/// file is gone are skipped.
fn legacy_listed(path: &Path) -> Result<Vec<String>> {
    let base = path.parent().ok_or("catalog has no parent")?;
    let listing = text(path, LIMIT)?;
    let mut lines = listing.lines();
    if lines.next() != Some("td-iso-catalog-v1") {
        return Err(format!(
            "{}: expected a td-iso-catalog-v1 catalog",
            path.display()
        ));
    }
    let mut signatures = Vec::new();
    for line in lines {
        if let Some(signature) = line.split('\t').nth(2) {
            match text(&base.join(signature), SMALL) {
                Ok(value) => signatures.push(hash(value.as_bytes())),
                Err(e) => println!("ISO catalog import: not comparing {signature}: {e}"),
            }
        }
    }
    Ok(signatures)
}

/// Move an imported legacy file aside, so it is never read again.
fn set_aside(path: &Path) -> Result<()> {
    if !io(path.try_exists())? {
        return Ok(());
    }
    let mut name = path.as_os_str().to_owned();
    name.push(".imported");
    io(fs::rename(path, PathBuf::from(name)))
}

/// `import-iso-catalog-s3 ROOT CATALOG RETIRED OUTPUT S3...`: seed a bucket
/// that holds no catalog from a host's `td-iso-catalog-v1` file, its
/// pending retirements (`td-iso-retired-v1`; a missing file is none) and
/// the `source-digests` record `./publish` kept beside each listed ISO,
/// then set both files aside. Where the bucket holds a catalog already, a
/// local file it does not cover refuses; one it covers is set aside.
pub(in crate::release) fn import(words: &[&str]) -> Result<()> {
    let [root, catalog_path, retired_path, output, s3_words @ ..] = words else {
        return Err(HELP.into());
    };
    let output = Path::new(output);
    directory(output)?;
    let catalog_path = Path::new(catalog_path);
    let retired_path = Path::new(retired_path);
    let (mut client, chain) = open(Path::new(root), s3_words)?;
    let local = io(catalog_path.try_exists())?;
    if let Some(remote) = client.get(KEY, LIMIT)? {
        if !local {
            return Ok(());
        }
        let bucket = parse_remote(&remote)?;
        let missing = legacy_listed(catalog_path)?
            .iter()
            .filter(|signature| !bucket.rows.iter().any(|r| r.signature == **signature))
            .count();
        if missing > 0 {
            return Err(format!(
                "the bucket's ISO catalog lacks {missing} release(s) {} lists: another host imported first, or this file is stale. See net/OPERATIONS.md, \"Per-user setup\", before changing either",
                catalog_path.display()
            ));
        }
        set_aside(catalog_path)?;
        set_aside(retired_path)?;
        println!(
            "ISO catalog: the bucket's covers {}; set it aside",
            catalog_path.display()
        );
        return Ok(());
    }
    if !local {
        return Err(format!(
            "the bucket holds no ISO catalog and {} does not exist; create a header-only one to start an empty listing (net/OPERATIONS.md)",
            catalog_path.display()
        ));
    }
    let listing = text(catalog_path, LIMIT)?;
    let base = catalog_path.parent().ok_or("catalog has no parent")?;
    let mut lines = listing.lines();
    if lines.next() != Some("td-iso-catalog-v1") {
        return Err("expected a td-iso-catalog-v1 catalog".into());
    }
    let mut catalog = Catalog { rows: Vec::new() };
    let lines: Vec<&str> = lines.collect();
    for (position, line) in lines.iter().enumerate() {
        let mut legacy = legacy(&chain, base, line, true)?;
        // The pre-bucket flow listed a release locally before uploading it,
        // so an interrupted publish leaves its last row unuploaded. Any
        // earlier row the bucket lacks refuses, before anything is uploaded:
        // the file is stale.
        let uploaded = match record(&client, &chain, &legacy.row)? {
            Some((record, _)) => present(&client, &record)?,
            None => false,
        };
        if !uploaded && position + 1 < lines.len() {
            return Err(format!(
                "listed {} is missing from the bucket: {} is stale (net/OPERATIONS.md, \"Per-user setup\")",
                legacy.row.signature_key(),
                catalog_path.display()
            ));
        }
        if !uploaded {
            let mut verified = verify(
                &legacy.root,
                &legacy.row.profile,
                &legacy.signature,
                &legacy.image,
                client.deadline(),
            )?;
            super::publish(&mut client, &mut verified, &legacy.row.profile)?;
        }
        legacy.row.digests = legacy_digests(&mut client, &legacy.image)?;
        catalog.push(legacy.row)?;
    }
    if io(retired_path.try_exists())? {
        let pending = text(retired_path, LIMIT)?;
        let mut lines = pending.lines();
        if lines.next() != Some("td-iso-retired-v1") {
            return Err("expected a td-iso-retired-v1 list".into());
        }
        for line in lines {
            match legacy(&chain, base, line, false) {
                // One listed again is no longer retired.
                Ok(legacy) if catalog.rows.iter().any(|r| r.signature == legacy.row.signature) => {}
                Ok(legacy) => catalog.push(legacy.row)?,
                Err(e) => println!(
                    "ISO catalog import: skipped a pending retirement ({e}); delete its files by hand (net/OPERATIONS.md, \"Storage retention\")"
                ),
            }
        }
    }
    // Every listed release must be listable from the bucket alone.
    entries(&client, &chain, &catalog)?;
    store(&client, output, &catalog, None)?;
    set_aside(catalog_path)?;
    set_aside(retired_path)?;
    let listed = catalog.listed().count();
    println!(
        "ISO catalog: imported {listed} listed and {} retired rows; set {} aside",
        catalog.rows.len() - listed,
        catalog_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Host {
        dir: crate::test_temp::TempDir,
        root: PathBuf,
        key: PathBuf,
        credentials: PathBuf,
    }

    fn host() -> Host {
        let dir = crate::test_temp::TempDir::new("iso-catalog").unwrap();
        let root = dir.join("root");
        let root_key = dir.join("root.key");
        let key = dir.join("release.key");
        let public = dir.join("release.pub");
        fs::write(&root_key, sig::keygen().unwrap().0).unwrap();
        let (secret, bytes) = sig::keygen().unwrap();
        fs::write(&key, secret).unwrap();
        fs::set_permissions(&key, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&public, sig::to_hex(&bytes)).unwrap();
        publisher::root(
            1,
            now().unwrap() + 365 * format::DAY,
            &root_key,
            &public,
            &root_key,
            &root,
        )
        .unwrap();
        let credentials = dir.join("credentials");
        s3::tests::write_credentials(&credentials);
        Host {
            dir,
            root,
            key,
            credentials,
        }
    }

    struct Release {
        signature: PathBuf,
        image: PathBuf,
        digests: PathBuf,
        output: PathBuf,
    }

    /// A signed ISO `name` holding `bytes`, and a digest record of `pins`.
    fn release(h: &Host, name: &str, bytes: &[u8], pins: &[&str]) -> Release {
        let dir = h.dir.join(name);
        fs::create_dir(&dir).unwrap();
        let image = dir.join(format!("{name}.iso"));
        fs::write(&image, bytes).unwrap();
        let signature = sign(
            &h.root,
            &h.key,
            "system-x86-64",
            &"a".repeat(40),
            &image,
            &dir.join("signatures"),
        )
        .unwrap();
        let digests = dir.join("source-digests");
        let mut record = format!("{DIGESTS_HEADER}\n{}\n", pins.len());
        for pin in pins {
            record.push_str(pin);
            record.push('\n');
        }
        fs::write(&digests, record).unwrap();
        let output = dir.join("out");
        fs::create_dir(&output).unwrap();
        fs::set_permissions(&output, fs::Permissions::from_mode(0o700)).unwrap();
        Release {
            signature,
            image,
            digests,
            output,
        }
    }

    fn bucket(h: &Host, mock: &s3::tests::Mock) -> Vec<String> {
        vec![
            mock.endpoint.clone(),
            "auto".into(),
            "td-releases".into(),
            h.credentials.to_str().unwrap().into(),
            "test".into(),
        ]
    }

    fn run_list(
        h: &Host,
        mock: &s3::tests::Mock,
        r: &Release,
        keep: usize,
        check: bool,
    ) -> Result<()> {
        let mut words: Vec<String> = vec![
            h.root.to_str().unwrap().into(),
            "system-x86-64".into(),
            r.signature.to_str().unwrap().into(),
            r.image.to_str().unwrap().into(),
            r.digests.to_str().unwrap().into(),
            keep.to_string(),
            r.output.to_str().unwrap().into(),
        ];
        words.extend(bucket(h, mock));
        if check {
            words.push("--check".into());
        }
        list(&words.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn run_retire(h: &Host, mock: &s3::tests::Mock, output: &Path, delete: bool) -> Result<()> {
        let mut words: Vec<String> = vec![
            h.root.to_str().unwrap().into(),
            output.to_str().unwrap().into(),
        ];
        words.extend(bucket(h, mock));
        if delete {
            words.extend(["--delete".into(), "--exclusive-publisher".into()]);
        }
        retire(&words.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn stored(bytes: &[u8], digest: &str) -> s3::tests::Stored {
        s3::tests::Stored {
            bytes: bytes.to_vec(),
            headers: BTreeMap::from([
                ("x-amz-meta-sha256".to_string(), digest.to_string()),
                ("cache-control".to_string(), s3::IMMUTABLE.to_string()),
            ]),
        }
    }

    fn empty(mock: &s3::tests::Mock) {
        mock.store.lock().unwrap().files.insert(
            KEY.into(),
            s3::tests::Stored {
                bytes: format!("{HEADER}\n").into_bytes(),
                headers: Default::default(),
            },
        );
    }

    fn remote_catalog(mock: &s3::tests::Mock) -> Catalog {
        let bytes = mock.store.lock().unwrap().files[KEY].bytes.clone();
        Catalog::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
    }

    fn page(mock: &s3::tests::Mock) -> String {
        String::from_utf8(mock.store.lock().unwrap().files["index.html"].bytes.clone()).unwrap()
    }

    fn row(listed: bool, profile: &str, n: char) -> Row {
        Row {
            listed,
            root: 1,
            profile: profile.into(),
            signature: n.to_string().repeat(64),
            digests: None,
        }
    }

    #[test]
    fn rows_round_trip_and_refuse_malformed_text() {
        let mut catalog = Catalog { rows: Vec::new() };
        catalog.push(row(true, "system-x86-64", 'a')).unwrap();
        let mut with_digests = row(false, "server-x86-64", 'b');
        with_digests.digests = Some("c".repeat(64));
        catalog.push(with_digests).unwrap();
        assert_eq!(Catalog::parse(&catalog.text()).unwrap(), catalog);
        assert!(catalog.push(row(false, "system-x86-64", 'a')).is_err());
        let line = row(true, "system-x86-64", 'd').line();
        for bad in [
            String::new(),
            "td-iso-catalog-v1\n".into(),
            format!("{HEADER}\n{line}{line}"),
            format!("{HEADER}\n{}", line.replace("listed", "pending")),
            format!("{HEADER}\n{}", line.replace("\t1\t", "\t0\t")),
            format!("{HEADER}\n{}", line.replace("system-x86-64", "arm")),
            format!("{HEADER}\n{}", line.trim_end()),
        ] {
            assert!(Catalog::parse(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn admission_keeps_the_newest_rows_of_each_profile() {
        let mut catalog = Catalog { rows: Vec::new() };
        catalog.push(row(true, "server-x86-64", 'e')).unwrap();
        for n in ['1', '2', '3'] {
            admit(&mut catalog, row(true, "system-x86-64", n), 2).unwrap();
        }
        let listed: Vec<char> = catalog
            .listed()
            .map(|r| r.signature.chars().next().unwrap())
            .collect();
        assert_eq!(listed, ['e', '2', '3']);
        assert!(!catalog.rows[1].listed);
        // A retry changes nothing; a row older than the newest kept cannot
        // be listed again over them.
        let before = catalog.text();
        admit(&mut catalog, row(true, "system-x86-64", '3'), 2).unwrap();
        assert_eq!(catalog.text(), before);
        assert!(admit(&mut catalog, row(true, "system-x86-64", '2'), 1).is_err());
        // A retired row listed again moves to the end.
        let mut catalog = Catalog::parse(&before).unwrap();
        admit(&mut catalog, row(true, "system-x86-64", '1'), 3).unwrap();
        let last = catalog.rows.last().unwrap();
        assert!(last.listed && last.signature.starts_with('1'));
    }

    #[test]
    fn digest_records_refuse_truncation() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let record = format!("{DIGESTS_HEADER}\n2\n{a}\n{b}\n");
        assert_eq!(parse_digests(&record).unwrap(), [a.as_str(), b.as_str()]);
        for bad in [
            String::new(),
            format!("{DIGESTS_HEADER}\n2\n{a}\n"),
            format!("{DIGESTS_HEADER}\n0\n"),
            format!("{DIGESTS_HEADER}\n1\nNOT-A-DIGEST\n"),
            "a-different-header\n1\n".into(),
        ] {
            assert!(parse_digests(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn hosts_continue_one_listing_and_retirement_deletes_what_it_trimmed() {
        let h = host();
        let mock = s3::tests::Mock::new();
        let pin = |c: char| c.to_string().repeat(64);
        let one = release(&h, "td-1", b"first ISO", &[&pin('a'), &pin('c')]);
        // Nothing to continue: the bucket must hold a catalog.
        let error = run_list(&h, &mock, &one, 2, false).unwrap_err();
        assert!(error.contains("holds no ISO catalog"), "{error}");
        empty(&mock);
        run_list(&h, &mock, &one, 2, true).unwrap();
        assert!(!mock.store.lock().unwrap().files.contains_key("index.html"));
        assert_eq!(remote_catalog(&mock).rows.len(), 0);
        run_list(&h, &mock, &one, 2, false).unwrap();
        let two = release(&h, "td-2", b"second ISO", &[&pin('a')]);
        run_list(&h, &mock, &two, 2, false).unwrap();
        assert!(page(&mock).contains("td-1.iso") && page(&mock).contains("td-2.iso"));
        // Another host, with none of td-1 or td-2 on disk, lists a third.
        fs::remove_dir_all(h.dir.join("td-1")).unwrap();
        fs::remove_dir_all(h.dir.join("td-2")).unwrap();
        let three = release(&h, "td-3", b"third ISO", &[&pin('b')]);
        run_list(&h, &mock, &three, 2, false).unwrap();
        let listing = page(&mock);
        assert!(!listing.contains("td-1.iso"));
        assert!(listing.contains("td-2.iso") && listing.contains("td-3.iso"));
        let catalog = remote_catalog(&mock);
        assert_eq!(catalog.rows.len(), 3);
        assert!(!catalog.rows[0].listed);

        let image = format!("iso/{}/td-1.iso", hash(b"first ISO"));
        for c in ['a', 'b', 'c'] {
            let digest = pin(c);
            mock.store
                .lock()
                .unwrap()
                .files
                .insert(format!("sources/{digest}"), stored(b"source", &digest));
        }
        run_retire(&h, &mock, &three.output, false).unwrap();
        assert!(mock.store.lock().unwrap().files.contains_key(&image));
        assert_eq!(remote_catalog(&mock).rows.len(), 3);
        run_retire(&h, &mock, &three.output, true).unwrap();
        let files = mock.store.lock().unwrap().files.clone();
        assert!(!files.contains_key(&image));
        assert!(!files.contains_key(&catalog.rows[0].signature_key()));
        // td-1 alone pinned c; a and b stay for td-2 and td-3.
        assert!(!files.contains_key(&format!("sources/{}", pin('c'))));
        assert!(files.contains_key(&format!("sources/{}", pin('a'))));
        assert!(files.contains_key(&format!("sources/{}", pin('b'))));
        assert_eq!(remote_catalog(&mock).rows.len(), 2);
        run_retire(&h, &mock, &three.output, true).unwrap();
    }

    #[test]
    fn a_lost_page_is_rebuilt_from_the_catalog_and_a_shared_image_stays() {
        let h = host();
        let mock = s3::tests::Mock::new();
        empty(&mock);
        let pin = "a".repeat(64);
        let first = release(&h, "td-1", b"one ISO", &[&pin]);
        // A page that cannot be read stops the run before the catalog moves.
        mock.store.lock().unwrap().fail = Some(("index.html".into(), 503, 3));
        assert!(run_list(&h, &mock, &first, 1, false).is_err());
        assert_eq!(remote_catalog(&mock).listed().count(), 0);
        run_list(&h, &mock, &first, 1, false).unwrap();
        assert!(page(&mock).contains("td-1.iso"));
        // A page lost after its catalog was written is rebuilt by a rerun.
        mock.store.lock().unwrap().files.remove("index.html");
        run_list(&h, &mock, &first, 1, false).unwrap();
        assert!(page(&mock).contains("td-1.iso"));
        assert_eq!(remote_catalog(&mock).rows.len(), 1);

        // The same image signed again replaces the first row; retirement
        // only unlists the old signature, since the image is still listed.
        let signature = sign(
            &h.root,
            &h.key,
            "system-x86-64",
            &"b".repeat(40),
            &first.image,
            &h.dir.join("td-1/resigned"),
        )
        .unwrap();
        let again = Release {
            signature,
            image: first.image.clone(),
            digests: first.digests.clone(),
            output: first.output.clone(),
        };
        run_list(&h, &mock, &again, 1, false).unwrap();
        run_retire(&h, &mock, &first.output, true).unwrap();
        let first_signature = format!(
            "iso/signatures/{}.sig",
            hash(&fs::read(&first.signature).unwrap())
        );
        let files = mock.store.lock().unwrap().files.clone();
        assert!(files.contains_key(&format!("iso/{}/td-1.iso", hash(b"one ISO"))));
        assert!(files.contains_key(&first_signature));
        assert_eq!(remote_catalog(&mock).rows.len(), 1);
    }

    #[test]
    fn the_page_features_each_profiles_newest_signed_issue() {
        let h = host();
        let pin = "a".repeat(64);
        let old = release(&h, "zzz-old", b"old image", &[&pin]);
        let new = release(&h, "aaa-new", b"new image", &[&pin]);
        let server_image = h.dir.join("server.iso");
        fs::write(&server_image, b"server image").unwrap();
        let server = sign(
            &h.root,
            &h.key,
            "server-x86-64",
            &"b".repeat(40),
            &server_image,
            &h.dir.join("server-signatures"),
        )
        .unwrap();
        // Backdate the lexically last image, so issue time, not the name or
        // the row order, decides which is featured.
        let signed = fs::read_to_string(&old.signature).unwrap();
        let mut lines: Vec<String> = signed.lines().map(str::to_string).collect();
        lines[2] = (now().unwrap() - 60).to_string();
        lines[3] = (lines[3].parse::<u64>().unwrap() - 60).to_string();
        lines.pop();
        let backdated =
            format::signed(&(lines.join("\n") + "\n"), &fs::read(&h.key).unwrap()).unwrap();
        let root = Root::parse(&text(&h.root, SMALL).unwrap(), None).unwrap();
        let entry = |profile: &str, signature: String| index::Entry {
            profile: profile.into(),
            root: 1,
            record: Record::parse(&signature, &root, profile, now().unwrap()).unwrap(),
            signature,
        };
        let new_text = fs::read_to_string(&new.signature).unwrap();
        let server_text = fs::read_to_string(&server).unwrap();
        let mut entries = vec![
            entry("system-x86-64", new_text.clone()),
            entry("system-x86-64", backdated),
            entry("server-x86-64", server_text),
        ];
        let html = index::render(&mut entries).unwrap();
        let (latest, previous) = html
            .split_once("<details class=\"archive\" id=\"previous-versions\">")
            .unwrap();
        assert!(latest.contains("aaa-new.iso") && !latest.contains("zzz-old.iso"));
        assert!(previous.contains("zzz-old.iso"));
        assert_eq!(html.matches("<h2>Latest release</h2>").count(), 2);
        assert!(html.contains("Server · x86-64") && html.contains("Desktop · x86-64"));
        assert!(html.contains(&format!("href=\"/iso/{}/aaa-new.iso\"", hash(b"new image"))));
        let mut too_many: Vec<_> = (0..=index::MAX_ENTRIES)
            .map(|_| entry("system-x86-64", new_text.clone()))
            .collect();
        assert!(index::render(&mut too_many).is_err());
        assert!(index::render(&mut [])
            .unwrap()
            .contains("No installer images"));
    }

    fn run_import(
        h: &Host,
        mock: &s3::tests::Mock,
        local: &Path,
        retired: &Path,
        output: &Path,
    ) -> Result<()> {
        let mut words: Vec<String> = vec![
            h.root.to_str().unwrap().into(),
            local.to_str().unwrap().into(),
            retired.to_str().unwrap().into(),
            output.to_str().unwrap().into(),
        ];
        words.extend(bucket(h, mock));
        import(&words.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn run_unlist(
        h: &Host,
        mock: &s3::tests::Mock,
        signatures: &[&str],
        output: &Path,
    ) -> Result<()> {
        let mut words: Vec<String> = vec![
            h.root.to_str().unwrap().into(),
            output.to_str().unwrap().into(),
        ];
        words.extend(bucket(h, mock));
        words.extend(signatures.iter().map(|s| s.to_string()));
        unlist(&words.iter().map(String::as_str).collect::<Vec<_>>())
    }

    fn upload(h: &Host, mock: &s3::tests::Mock, r: &Release) {
        let mut client = mock.client(&h.credentials);
        let mut verified = verify(
            &h.root,
            "system-x86-64",
            &r.signature,
            &r.image,
            client.deadline(),
        )
        .unwrap();
        super::super::publish(&mut client, &mut verified, "system-x86-64").unwrap();
    }

    #[test]
    fn the_check_covers_this_release_and_unlist_retires_one_row() {
        let h = host();
        let mock = s3::tests::Mock::new();
        empty(&mock);
        let pin = "a".repeat(64);
        let first = release(&h, "td-1", b"one ISO", &[&pin]);
        run_list(&h, &mock, &first, 2, false).unwrap();
        let signature = sign(
            &h.root,
            &h.key,
            "system-x86-64",
            &"b".repeat(40),
            &first.image,
            &h.dir.join("td-1/resigned"),
        )
        .unwrap();
        let again = Release {
            signature,
            image: first.image.clone(),
            digests: first.digests.clone(),
            output: first.output.clone(),
        };
        // Both rows would stay listed: the check refuses before promotion
        // rather than the listing after it.
        let keys = |mock: &s3::tests::Mock| {
            mock.store
                .lock()
                .unwrap()
                .files
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        let before = keys(&mock);
        let error = run_list(&h, &mock, &again, 2, true).unwrap_err();
        assert!(error.contains("twice"), "{error}");
        assert_eq!(keys(&mock), before);
        assert!(run_list(&h, &mock, &again, 2, false).is_err());

        let first_row = hash(&fs::read(&first.signature).unwrap());
        assert!(run_unlist(&h, &mock, &[&"f".repeat(64)], &first.output).is_err());
        run_unlist(&h, &mock, &[&first_row], &first.output).unwrap();
        assert!(!page(&mock).contains("td-1.iso"));
        assert!(!remote_catalog(&mock).rows[0].listed);
        run_list(&h, &mock, &again, 2, true).unwrap();
        run_list(&h, &mock, &again, 2, false).unwrap();
        assert!(page(&mock).contains("td-1.iso"));

        // Another host's write refuses a stale view of the catalog.
        let client = mock.client(&h.credentials);
        let (remote, mut stale) = fetch(&client).unwrap();
        let (_, mut other) = fetch(&client).unwrap();
        other.rows.retain(|r| r.listed);
        store(&client, &first.output, &other, Some(&remote)).unwrap();
        assert!(current(&client, &remote.bytes).is_err());
        stale.push(row(false, "server-x86-64", 'c')).unwrap();
        assert!(store(&client, &first.output, &stale, Some(&remote)).is_err());
        assert_eq!(remote_catalog(&mock), other);
    }

    #[test]
    fn unlist_clears_several_broken_rows_in_one_write() {
        let h = host();
        let mock = s3::tests::Mock::new();
        empty(&mock);
        let pin = "a".repeat(64);
        let one = release(&h, "td-1", b"one ISO", &[&pin]);
        let two = release(&h, "td-2", b"two ISO", &[&pin]);
        run_list(&h, &mock, &one, 5, false).unwrap();
        run_list(&h, &mock, &two, 5, false).unwrap();
        // Both images removed by hand: every listing now refuses.
        for (name, bytes) in [("td-1", b"one ISO"), ("td-2", b"two ISO")] {
            let key = format!("iso/{}/{name}.iso", hash(bytes));
            mock.store.lock().unwrap().files.remove(&key);
        }
        let sha = |r: &Release| hash(&fs::read(&r.signature).unwrap());
        let (first, second) = (sha(&one), sha(&two));
        assert!(run_unlist(&h, &mock, &[&first], &one.output).is_err());
        run_unlist(&h, &mock, &[&first, &second], &one.output).unwrap();
        assert_eq!(remote_catalog(&mock).listed().count(), 0);
        assert!(page(&mock).contains("No installer images"));
        // Retirement then finds the images absent and drops the rows; with
        // nothing listed, mirror cleanup refuses an empty keep set.
        run_retire(&h, &mock, &one.output, true).unwrap_err();
        assert!(remote_catalog(&mock).rows.is_empty());
    }

    #[test]
    fn import_seeds_an_empty_bucket_once_from_a_local_catalog() {
        let h = host();
        let mock = s3::tests::Mock::new();
        let pin = |c: char| c.to_string().repeat(64);
        let old = release(&h, "td-0", b"old ISO", &[&pin('c')]);
        let gone = release(&h, "td-g", b"gone ISO", &[&pin('c')]);
        let undigested = release(&h, "td-u", b"undigested ISO", &[&pin('a')]);
        let kept = release(&h, "td-1", b"kept ISO", &[&pin('a')]);
        fs::remove_file(&undigested.digests).unwrap();
        for r in [&old, &gone, &undigested] {
            upload(&h, &mock, r);
        }
        // `kept` was listed locally by an interrupted publish, never uploaded.
        let local_row = |r: &Release| {
            format!(
                "{}\tsystem-x86-64\t{}\t{}\n",
                h.root.display(),
                r.signature.display(),
                r.image.display()
            )
        };
        let local = h.dir.join("iso-catalog.tsv");
        let retired = h.dir.join("retired-isos.tsv");
        let output = &kept.output;
        let error = run_import(&h, &mock, &local, &retired, output).unwrap_err();
        assert!(error.contains("does not exist"), "{error}");
        // Only the last row may be one an interrupted publish never
        // uploaded; an earlier one the bucket lacks means a stale file.
        fs::write(
            &local,
            format!(
                "td-iso-catalog-v1\n{}{}",
                local_row(&kept),
                local_row(&kept)
            ),
        )
        .unwrap();
        let error = run_import(&h, &mock, &local, &retired, output).unwrap_err();
        assert!(error.contains("missing from the bucket"), "{error}");
        let kept_image = format!("iso/{}/td-1.iso", hash(b"kept ISO"));
        let files = mock.store.lock().unwrap().files.clone();
        assert!(!files.contains_key(KEY) && !files.contains_key(&kept_image));
        assert!(local.exists());
        fs::write(
            &local,
            format!(
                "td-iso-catalog-v1\n{}{}",
                local_row(&undigested),
                local_row(&kept)
            ),
        )
        .unwrap();
        fs::write(
            &retired,
            format!("td-iso-retired-v1\n{}{}", local_row(&old), local_row(&gone)),
        )
        .unwrap();
        // A pending row whose signature file is gone is skipped, not fatal.
        fs::remove_file(&gone.signature).unwrap();
        run_import(&h, &mock, &local, &retired, output).unwrap();
        let catalog = remote_catalog(&mock);
        let listed: Vec<bool> = catalog.rows.iter().map(|r| r.listed).collect();
        assert_eq!(listed, [true, true, false]);
        assert_eq!(catalog.rows[0].digests, None);
        let digests = hash(&fs::read(&kept.digests).unwrap());
        assert_eq!(catalog.rows[1].digests.as_deref(), Some(digests.as_str()));
        let files = mock.store.lock().unwrap().files.clone();
        assert!(files.contains_key(&format!("{DIGESTS}/{digests}")));
        assert!(files.contains_key(&format!("iso/{}/td-1.iso", hash(b"kept ISO"))));
        // Both local files are set aside, so they are never read again.
        assert!(!local.exists() && !retired.exists());
        assert!(h.dir.join("iso-catalog.tsv.imported").exists());
        run_import(&h, &mock, &local, &retired, output).unwrap();
        assert_eq!(remote_catalog(&mock), catalog);

        // A host whose listing the bucket's catalog lacks refuses.
        let stale = release(&h, "td-s", b"stale ISO", &[&pin('a')]);
        fs::write(&local, format!("td-iso-catalog-v1\n{}", local_row(&stale))).unwrap();
        let error = run_import(&h, &mock, &local, &retired, output).unwrap_err();
        assert!(error.contains("lacks 1 release"), "{error}");
        assert!(local.exists());
        fs::write(&local, format!("td-iso-catalog-v1\n{}", local_row(&kept))).unwrap();
        run_import(&h, &mock, &local, &retired, output).unwrap();
        assert!(!local.exists());
        assert_eq!(remote_catalog(&mock), catalog);

        // Retirement finishes the old host's pending row; mirror cleanup
        // waits while a listed release has no digest record.
        let only_old = format!("sources/{}", pin('c'));
        mock.store
            .lock()
            .unwrap()
            .files
            .insert(only_old.clone(), stored(b"source", &pin('c')));
        run_retire(&h, &mock, output, true).unwrap();
        assert_eq!(remote_catalog(&mock).rows.len(), 2);
        let files = mock.store.lock().unwrap().files.clone();
        assert!(!files.contains_key(&format!("iso/{}/td-0.iso", hash(b"old ISO"))));
        assert!(files.contains_key(&only_old));
    }
}
