//! Explicit local catalog; the generated page is navigation, never a trust anchor.
use super::*;
use std::collections::BTreeMap;

const LIMIT: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 128;
struct Entry {
    profile: String,
    verified: Verified,
}
struct Plan {
    entries: Vec<Entry>,
    inputs: Vec<PathBuf>,
}
impl Plan {
    fn load(catalog: &Path, deadline: Instant) -> Result<Self> {
        let catalog_text = text(catalog, LIMIT)?;
        let mut lines = catalog_text.lines();
        if lines.next() != Some("td-iso-catalog-v1") {
            return Err("expected td-iso-catalog-v1 catalog".into());
        }
        let base = catalog.parent().ok_or("catalog has no parent")?;
        let mut inputs = vec![io(fs::canonicalize(catalog))?];
        let mut entries = Vec::new();
        let mut roots = BTreeMap::new();
        let mut images = BTreeMap::new();
        for line in lines {
            if entries.len() == MAX_ENTRIES {
                return Err("ISO catalog exceeds 128 entries".into());
            }
            let fields: Vec<_> = line.split('\t').collect();
            let [root, profile, signature, image] = fields.as_slice() else {
                return Err(
                    "catalog row needs ROOT, PROFILE, SIGNATURE, ISO separated by tabs".into(),
                );
            };
            if fields
                .iter()
                .any(|f| f.is_empty() || f.chars().any(char::is_control))
            {
                return Err("empty or control-containing ISO catalog field".into());
            }
            let root = base.join(root);
            let signature = base.join(signature);
            let image = base.join(image);
            let verified = verify(&root, profile, &signature, &image, deadline)?;
            if roots
                .insert(verified.root.version, verified.root.text.clone())
                .is_some_and(|old| old != verified.root.text)
            {
                return Err("catalog contains conflicting roots for one epoch".into());
            }
            if images
                .insert((profile.to_string(), verified.record.image_key()), ())
                .is_some()
            {
                return Err("duplicate ISO/profile in catalog; select one signature".into());
            }
            for path in [root, signature, image] {
                inputs.push(io(fs::canonicalize(path))?);
            }
            entries.push(Entry {
                profile: profile.to_string(),
                verified,
            });
        }
        entries.sort_by(|a, b| {
            a.profile
                .cmp(&b.profile)
                .then(b.verified.record.issued.cmp(&a.verified.record.issued))
                .then(a.verified.record.name.cmp(&b.verified.record.name))
                .then(a.verified.record.digest.cmp(&b.verified.record.digest))
        });
        Ok(Self { entries, inputs })
    }
    fn fresh(&self) -> Result<()> {
        let time = now()?;
        for entry in &self.entries {
            Record::parse(
                &entry.verified.signature,
                &entry.verified.root,
                &entry.profile,
                time,
            )?;
        }
        Ok(())
    }
    fn render(&self) -> Result<String> {
        self.fresh()?;
        let mut page = String::from(include_str!("index-start.html"));
        page.push_str("<style>\n");
        page.push_str(include_str!("index.css"));
        page.push_str("</style></head><body><a class=\"skip-link\" href=\"#main\">Skip to content</a>\n<header class=\"site-header wrap\"><a class=\"wordmark\" href=\"https://www.tdlinux.org/\" aria-label=\"td home\">td<span aria-hidden=\"true\">.</span></a><nav aria-label=\"Main navigation\"><a href=\"https://www.tdlinux.org/\">About td</a><a href=\"https://github.com/timmydo/td\">Source ↗</a></nav></header>\n<main id=\"main\" class=\"wrap\"><section class=\"hero\"><p class=\"eyebrow\">Timmy’s Distro</p><h1>Download <span>td.</span></h1><p class=\"hero-note\">Installer images for a desktop of your own.<br>A personal project, still taking shape.</p></section>\n<section aria-label=\"Installer images\">\n");
        if self.entries.is_empty() {
            page.push_str("<p class=\"empty\">No installer images are currently listed.</p>\n");
        }
        let mut latest = Vec::new();
        let mut previous = Vec::new();
        let mut profile = None;
        for entry in &self.entries {
            if profile != Some(entry.profile.as_str()) {
                latest.push(entry);
                profile = Some(entry.profile.as_str());
            } else {
                previous.push(entry);
            }
        }
        for entry in latest {
            render_entry(&mut page, entry, true);
        }
        if !previous.is_empty() {
            page.push_str("<details class=\"archive\" id=\"previous-versions\"><summary>Previous versions</summary><p class=\"archive-note\">Older installer images, retained for earlier installations.</p>\n");
            for entry in previous {
                render_entry(&mut page, entry, false);
            }
            page.push_str("</details>\n");
        }
        page.push_str("</section><section class=\"verify\"><h2>Before you install</h2><p>Verify the ISO with its signature and an independently trusted public root before writing it to a USB drive. A root downloaded from this page alone does not establish trust.</p><p>Check the signing policy shown above; non-expiring signatures require no periodic renewal. This static listing is not proof that an image is current. Verification authenticates the download; it does not enable firmware Secure Boot.</p><a href=\"https://github.com/timmydo/td/blob/main/net/ISO.md\">Signature verification instructions ↗</a></section></main><footer class=\"site-footer wrap\"><p><strong>td</strong> Timmy’s Distro</p><a href=\"https://github.com/timmydo/td\">Made in the open ↗</a></footer></body></html>\n");
        if page.len() as u64 > LIMIT {
            return Err("generated ISO index exceeds one MiB".into());
        }
        Ok(page)
    }
    fn write(&self, output: &Path, page: &str) -> Result<File> {
        // Never replace catalog inputs, including aliases and hard links.
        match fs::metadata(output) {
            Ok(out) => {
                for path in &self.inputs {
                    let input = io(fs::metadata(path))?;
                    if out.dev() == input.dev() && out.ino() == input.ino() {
                        return Err("ISO index output aliases a catalog input".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.to_string()),
        }
        replace(output, page.as_bytes())?;
        let (mut file, meta) = io(td_fs::open_real_file(output, "generated ISO index"))?;
        if meta.len() != page.len() as u64
            || s3::file_hash_until(
                &mut file,
                meta.len(),
                Instant::now() + Duration::from_secs(60),
            )? != hash(page.as_bytes())
        {
            return Err("generated ISO index changed before opening".into());
        }
        Ok(file)
    }
}
fn render_entry(page: &mut String, entry: &Entry, latest: bool) {
    let v = &entry.verified;
    let r = &v.record;
    let profile = if entry.profile == "system-x86-64" {
        "Desktop · x86-64"
    } else {
        "Server · x86-64"
    };
    let expiry = if r.expires == 0 {
        "No expiry".to_string()
    } else {
        let date = date(r.expires);
        format!("<time datetime=\"{date}\">{date}</time>")
    };
    let issued = date(r.issued);
    let (class, heading) = if latest {
        ("latest", "Latest release")
    } else {
        ("previous", "Previous release")
    };
    // All interpolated fields have passed the signed format's restricted ASCII grammar.
    page.push_str(&format!("<article class=\"release {class}\"><div><p class=\"eyebrow\">{profile}</p><h2>{heading}</h2><p class=\"filename\">{}</p><p class=\"published\">Published <time datetime=\"{issued}\">{issued}</time></p><p class=\"size\">{:.1} MiB · {} bytes</p><a class=\"button\" href=\"/{}\">Download ISO <span aria-hidden=\"true\">↓</span></a><div class=\"links\"><a href=\"/iso/signatures/{}.sig\">Signature</a><a href=\"/roots/{}\">Public root</a></div></div><dl><dt>Source revision</dt><dd><a href=\"https://github.com/timmydo/td/commit/{}\"><code>{}</code></a></dd><dt>Signature expires (UTC)</dt><dd>{expiry}</dd><dt>SHA-256</dt><dd><code>{}</code></dd></dl></article>\n",r.name,r.size as f64 / 1048576.0,r.size,r.image_key(),hash(v.signature.as_bytes()),v.root.version,r.commit,r.commit,r.digest));
}
fn date(seconds: u64) -> String {
    let (year, month, day) = crate::utc::civil(seconds / 86400);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}
pub(in crate::release) fn generate(catalog: &Path, output: &Path) -> Result<()> {
    let plan = Plan::load(catalog, Instant::now() + Duration::from_secs(3600))?;
    let page = plan.render()?;
    plan.write(output, &page)?;
    println!(
        "generated {} with {} ISO entries",
        output.display(),
        plan.entries.len()
    );
    Ok(())
}
pub(in crate::release) fn upload(words: &[&str]) -> Result<()> {
    let (words, dry) = if words.last() == Some(&"--dry-run") {
        (words.get(..words.len() - 1).ok_or(HELP)?, true)
    } else {
        (words, false)
    };
    let [catalog, output, endpoint, region, bucket, credentials, profile] = words else {
        return Err(HELP.into());
    };
    let mut client = s3::Client::new(endpoint, region, bucket, Path::new(credentials), profile)?;
    let mut plan = Plan::load(Path::new(catalog), client.deadline())?;
    let page = plan.render()?;
    let mut file = plan.write(Path::new(output), &page)?;
    println!(
        "ISO index {}: {} entries, {} bytes",
        if dry { "dry run" } else { "upload" },
        plan.entries.len(),
        page.len()
    );
    if dry {
        return Ok(());
    }
    let previous = client.get("index.html", LIMIT)?;
    for entry in &mut plan.entries {
        publish(&mut client, &mut entry.verified, &entry.profile)?;
    }
    plan.fresh()?;
    client.html_index(&mut file, page.as_bytes(), previous.as_ref())?;
    println!(
        "ISO index published last: {} immutable files uploaded, {} reused",
        client.uploaded, client.reused
    );
    Ok(())
}
