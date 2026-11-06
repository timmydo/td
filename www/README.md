# www.tdlinux.org

Static homepage for **td: Timmy’s Distro**. Open `index.html` directly in a
browser, or serve this directory with any static HTTP server. No build,
JavaScript, package installation, external fonts, or third-party page resources
are required. Links to source and releases leave the site only when followed.

## Public files

Publish only these files, keeping their relative paths:

- `index.html` (`text/html; charset=utf-8`)
- `style.css` (`text/css; charset=utf-8`)
- `assets/favicon.svg` (`image/svg+xml`)
- `assets/td-desktop.jpg` (`image/jpeg`)

The desktop photo was supplied by Timmy for this homepage. It is resized to
960 × 695, preserving its aspect ratio, and encoded as a progressive JPEG
at quality 85 with full chroma resolution. Keep descriptive alternative text
and explicit dimensions when replacing it.

## Hosting handoff

The intended origin is `https://www.tdlinux.org/`. Image downloads and release
metadata belong to `https://releases.tdlinux.org/`, maintained separately.
The Download links scroll to this page's install section; that section links
to the release site's root rather than hard-coding an image version.

The R2/S3 uploader is being developed in the `image-updates-rolling`
workstream and is intentionally not duplicated here. Once available, reuse it
with the public-file allowlist above and a separate website bucket or prefix.
Do not upload this README, repository files, or credentials. Upload assets
before `index.html`; use revalidation for these unversioned file names.

The public serving layer must map `/` to `index.html` with an HTML content
type. Object upload alone does not establish that routing, the custom domain,
or cache behavior. Verify these when connecting the bucket to
`www.tdlinux.org`. No deployment or bucket configuration is performed by
these static files.
