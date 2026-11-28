//! The download page, rendered from the bucket's ISO catalog
//! (`catalog.rs`); the page is navigation, never a trust anchor.
use super::*;

const LIMIT: u64 = 1024 * 1024;
pub(super) const MAX_ENTRIES: usize = 128;

/// One listed installer: its verified record and signature text.
pub(super) struct Entry {
    pub(super) profile: String,
    pub(super) root: u64,
    pub(super) record: Record,
    pub(super) signature: String,
}

/// The page for `entries`, sorted by profile, then newest signed issue.
pub(super) fn render(entries: &mut [Entry]) -> Result<String> {
    if entries.len() > MAX_ENTRIES {
        return Err("ISO catalog exceeds 128 listed entries".into());
    }
    entries.sort_by(|a, b| {
        a.profile
            .cmp(&b.profile)
            .then(b.record.issued.cmp(&a.record.issued))
            .then(a.record.name.cmp(&b.record.name))
            .then(a.record.digest.cmp(&b.record.digest))
    });
    let mut page = String::from(include_str!("index-start.html"));
    page.push_str("<style>\n");
    page.push_str(include_str!("index.css"));
    page.push_str("</style></head><body><a class=\"skip-link\" href=\"#main\">Skip to content</a>\n<header class=\"site-header wrap\"><a class=\"wordmark\" href=\"https://www.tdlinux.org/\" aria-label=\"td home\">td<span aria-hidden=\"true\">.</span></a><nav aria-label=\"Main navigation\"><a href=\"https://www.tdlinux.org/\">About td</a><a href=\"https://github.com/timmydo/td\">Source ↗</a></nav></header>\n<main id=\"main\" class=\"wrap\"><section class=\"hero\"><p class=\"eyebrow\">Timmy’s Distro</p><h1>Download <span>td.</span></h1><p class=\"hero-note\">Installer images for a desktop of your own.<br>A personal project, still taking shape.</p></section>\n<section aria-label=\"Installer images\">\n");
    if entries.is_empty() {
        page.push_str("<p class=\"empty\">No installer images are currently listed.</p>\n");
    }
    let mut latest = Vec::new();
    let mut previous = Vec::new();
    let mut profile = None;
    for entry in entries.iter() {
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
fn render_entry(page: &mut String, entry: &Entry, latest: bool) {
    let r = &entry.record;
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
    page.push_str(&format!("<article class=\"release {class}\"><div><p class=\"eyebrow\">{profile}</p><h2>{heading}</h2><p class=\"filename\">{}</p><p class=\"published\">Published <time datetime=\"{issued}\">{issued}</time></p><p class=\"size\">{:.1} MiB · {} bytes</p><a class=\"button\" href=\"/{}\">Download ISO <span aria-hidden=\"true\">↓</span></a><div class=\"links\"><a href=\"/iso/signatures/{}.sig\">Signature</a><a href=\"/roots/{}\">Public root</a></div></div><dl><dt>Source revision</dt><dd><a href=\"https://github.com/timmydo/td/commit/{}\"><code>{}</code></a></dd><dt>Signature expires (UTC)</dt><dd>{expiry}</dd><dt>SHA-256</dt><dd><code>{}</code></dd></dl></article>\n",r.name,r.size as f64 / 1048576.0,r.size,r.image_key(),hash(entry.signature.as_bytes()),entry.root,r.commit,r.commit,r.digest));
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
