# Source mirror

td keeps a copy of every source and build-root pin's bytes on its
release site, so a source build (`./update`, `td-builder check`,
`./publish`) need not reach each pin's upstream host: gitlab.com's raw
endpoint answers 429 to a cold warm of the linux-firmware pins, which
are one request per file. Crate vendor
closures and `td-rust fetch` keep their own upstream fetches. The mirror is a
transport, not a trust root or a pin: it holds only bytes a pin already
names by SHA-256, and every pin keeps its reviewed upstream URL, which
the graph builds from when the mirror cannot answer. It is optional
infrastructure under `AGENTS.md` principle 5; nothing td produces is
stored there.

## Layout

A pin's bytes live at `sources/SHA256` under the mirror origin, the
pin's lowercase hex digest, beside the release repository's own keys
(`RELEASES.md`) in the same bucket. Release collection lists and
deletes only `releases/` indexes and `objects/` it can derive from the
local repository, so it never removes a source; only `./publish`'s
retention does (below). Objects are immutable and carry
`Cache-Control: public, max-age=31536000, immutable`.

## Fetching

`td-feed warm sources` and `td-feed warm build-root` take each pin not
already warm and verified, through the shared feed when one runs, from
`MIRROR/sources/SHA256` and then from the pin's URL. Each attempt is
checked against the pin's SHA-256 before it is published into the feed
store or `~/.td/sources`, so a missing, stale, corrupt or substituted
mirror object is a miss, reported on stderr, and the upstream attempt
follows. The shared feed stores a mirrored pin at its upstream URL's
path, and is asked for the pin whether or not this host could fill its
store, since an explicit `TD_FEED_BASE` may serve one it cannot.
Explicit consumers (`td-feed consume sources`) keep their single feed
and no fallback.

A mirror attempt refuses redirects, abandons a peer silent for the HTTP
read timeout (60 seconds, three attempts), and has a two-hour absolute
deadline against a trickle. Each warm gives the mirror a two-minute
budget of time lost to failed attempts, which a success never refunds;
once spent, that warm uses upstream for its remaining pins. A miss costs
one request on a fresh TLS connection, so an empty or partial mirror costs
a round trip or so per pin; on a high-latency link many misses in a row
(a firmware tag bump not yet published) can spend the budget and send
the rest of that warm upstream, which only forgoes the mirror. A
stalling or erroring mirror costs a warm about the budget plus one
attempt in flight: a few minutes for a silent one, and
at most the absolute deadline for one that trickles. Time spent waiting
for another warm's download lock counts too, which can only move a warm
to upstream early.

`TD_SOURCE_MIRROR` selects the origin. Unset, it is
`https://releases.tdlinux.org`; empty or `off` disables the mirror; any
other value must name one http(s) origin, as `TD_FEED_BASE` does.
td-recipe-eval passes its environment to the warm, so the variable
reaches `td-builder check` and a direct `td-feed` run; `./update`
clears its environment and always uses the default, bounded as above.

While the mirror holds a pin's bytes, a dead upstream URL for that pin
goes unnoticed. A pin bump whose digest is new is fetched upstream
until a publish uploads it. To prove every upstream URL, warm with the
mirror off into empty caches, for example with a fresh `HOME` and
`TD_FEED_NO_DAEMON` set: a warm skips any pin it already holds.

Every source warm asks the mirror origin first, so the operator of that
origin sees which pins a host fetches, as an upstream host sees the
pins it serves.

On a td system the warm runs the deployed `td-net`
(`td-update/DESIGN.md`, "Build command"), so a deployment fetches
through the mirror once it was built from a checkout that does.

## Publishing

`./publish` warms both full pin rosters and uploads them before it
signs or promotes anything (`OPERATIONS.md`), so a published release's
sources are mirrored. To fill the mirror by hand, warm the pins and
upload them with the publisher's bucket settings (`S3.md`):

```text
net/target/release/td-net feed warm sources
net/target/release/td-net feed warm build-root
net/target/release/td-net s3 sources ~/.td/sources ENDPOINT auto td-releases CREDENTIALS td-releases --dry-run
```

Remove `--dry-run` to upload. Run it from the checkout whose pins it
should publish: it resolves the source and build-root pin rosters
through that checkout's `td-recipe-eval`. Each pin's cached file is
opened without following links and must hash to its pin and fit the
S3 upload bound; a missing, mismatched or oversized file is reported,
counted and skipped, and the command fails once the rest are done.
Uploads use `s3 put`'s immutable rule: an object already present with
the same length, SHA-256 metadata and cache policy is reused, a
conflicting one refuses, and a rerun resumes. Each pin's upload has its
own one-hour deadline. `s3 put` refuses keys under `sources/`.

## Retention

Each publish records the digests it mirrored and then deletes every
`sources/` object that none of the releases it still lists pins
(`OPERATIONS.md`, "Storage retention"). A branch's pin that no listed
release names is fetched upstream:

```text
net/target/release/td-net s3 sources-gc KEEP ENDPOINT auto td-releases CREDENTIALS td-releases
```

KEEP lists one lowercase SHA-256 per line and may not be empty. Without
`--delete --exclusive-publisher` the command only plans: it lists
`sources/` with the bounded inventory release collection uses and
checks each candidate's upload metadata. With them it deletes each
candidate whose metadata still names its digest, with `If-Match` on the
observed ETag.

## Licences

The mirror redistributes upstream bytes under their own licences.
Firmware and font notices are pins of their own and are mirrored with
them. The build root's pins are Debian binary packages, GPL-licensed
ones among them; their corresponding source stays at
snapshot.debian.org, and the mirror's operator answers for that offer
as td's images already do for the glibc and libgcc they carry.
