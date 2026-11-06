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

## Upload

Build the shared Rust uploader once from the repository root:

```text
cargo build --release --locked --manifest-path net/Cargo.toml
```

Save S3 credentials outside the repository in an owner-only regular file
(mode 0600), in the format documented in [net/S3.md](../net/S3.md). Never put
keys in command arguments or in this directory. The launcher only passes
arguments to the shared Rust uploader; it contains no upload logic.

From the repository root, validate all files and credentials without making
network requests:

```text
./www/upload https://ACCOUNT_ID.r2.cloudflarestorage.com auto www /absolute/path/to/credentials www --dry-run
```

Remove `--dry-run` to upload. Arguments are endpoint, region, bucket,
credentials path, and credentials profile. Relative credential paths resolve
from the caller's directory. The equivalent Rust command is:

```text
net/target/release/td-net s3 website www ENDPOINT auto www CREDENTIALS PROFILE [--dry-run]
```

Only the four public files listed above are uploaded. The uploader sends
assets first and `index.html` last, uses each file's content type, and sets
`Cache-Control: no-cache` so these unversioned names must be revalidated.
Existing objects are conditionally replaced against their observed ETags;
identical content with matching upload metadata is reused. A conflicting
write stops publication. No objects are deleted. Publishing several fixed
paths is not an atomic deployment: a partial upload can leave some assets
updated; rerun after resolving a failure. See [S3.md](../net/S3.md) for bounds
and the trust placed in the service's metadata.

## Domain and root routing

The intended origin is `https://www.tdlinux.org/`. Image downloads and release
metadata belong to `https://releases.tdlinux.org/`, maintained separately.
The Download links scroll to this page's install section; that section links
to the release site's root rather than hard-coding an image version.

Connect `www.tdlinux.org` as the website bucket's R2 custom domain. The public
serving layer must map `/` to `index.html`; uploading files does not configure
that route. In Cloudflare Rules, create a URL Rewrite Rule with this filter:

```text
(http.host eq "www.tdlinux.org" and http.request.uri.path eq "/")
```

Rewrite the path statically to `/index.html` and preserve the query string.
Keep cache rules from overriding the uploaded revalidation policy. This
requires Cloudflare dashboard or zone API access; an S3 key only uploads
objects. Verify `/`, `/index.html`, `/style.css`, and both assets after
publication. [Cloudflare's rewrite instructions](https://developers.cloudflare.com/rules/transform/url-rewrite/create-dashboard/)
describe the dashboard configuration.
