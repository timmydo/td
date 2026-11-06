# Signed system image delivery

`td-net release` publishes and acquires complete deployment images through a
static repository. The repository may be local (`file:/absolute/path`),
loopback HTTP, or HTTPS. It needs no executing server component. There is no
default public repository or compiled publisher key. Source builds remain
available independently. This protocol authenticates a publisher's assertion
about an image and source commit; it does not prove reproducibility or that
the publisher applied security fixes. It is a td protocol, not TUF.

## Trust and freshness

The operator provisions a root document and profile out of band with
`td-net release init STATE ROOT PROFILE`. The supported profiles are
`system-x86-64` and `server-x86-64`. This is transport support: only the
desktop ships td-admin; stock servers retain replacement/reprovisioning.
State must be protected by its caller;
copying or deleting it loses replay protection. There is no trust-on-first-use
network key, reset flag, automatic source fallback or automatic key recovery.

An offline Ed25519 root key authorizes one release key. The release key signs
image indexes and short-lived channel heads. Keep the root private key away
from the publishing host. A root document contains its positive version,
expiration, root public key and release public key. Each rotation advances
exactly one version and is signed by both the previous and new root keys.
The old key signs the body; the new key signs that body and old signature.
An initially provisioned document needs its self-signature; its identity
comes from provisioning. A maximum of 64 sequential rotations is followed
per acquisition. Root documents remain in the repository indefinitely.

Head and release counters are scoped to the root version. An authenticated
root rotation resets those counters, so a compromised release key cannot
block its replacement by signing maximum versions. The clock floor remains.
Changing the release key in the next root revokes its predecessor for all
subsequent admissions under that root. Learned roots persist even if later
acquisition fails. An expired old root may authorize a sequential rotation;
the final root must be unexpired. A client cannot discover a withheld
rotation by cryptography alone. Compromise of the release key permits fresh
forgeries for clients still trusting its old root, until they learn the
rotation or that root expires. Lost or compromised root-key recovery needs
trusted out-of-band reprovisioning.

An image index binds its root version, positive release sequence, issue and
expiry times, profile, 40-hex source commit, exact deployment manifest, and
the ordered object list and full SHA-256 for each payload. Its maximum
lifetime is 30 days. A channel head binds its root version, positive head
version, issue and expiry times, profile and index SHA-256. Its maximum
lifetime is 24 hours. All signed bodies start with distinct versioned format
headers. Numbers, lowercase hex, whitespace and newlines are canonical.
Unknown fields and trailing records are refused.

Client state retains root metadata, head version and digest, release sequence
and digest, profile and the last accepted wall-clock time. Lower versions,
same-version/different-content metadata, future issue times, expiration and
clock rollback refuse. Repeating identical metadata is allowed for retries.
This needs a reasonably correct wall clock at initial provisioning; it does
not establish secure time. Suppressing new releases can freeze a client only
until its signed head expires when the signing key is uncompromised. State
advances before object acquisition, so an unsuccessful acquisition does not
restore permission to accept an earlier release.

## Bytes, objects and cache

The default publisher uses Gear content-defined chunks over the stored
bytes of all three payloads, including compressed EROFS bytes. The shared
`engine/src/image_chunks.rs` also supplies the offline comparison prototype:
16 KiB minimum, a 64 KiB boundary mask, 256 KiB maximum, reset at each cut.
There is no claim that a chunk corresponds to a file. A short final chunk
is allowed. EROFS extent-based reuse remains an experimental comparator in
`td-image-transfer`, not an alternative production index parser.

The index is at most 16 MiB with at most 200,000 object records in total.
Payload maxima are 256 MiB for bzImage, 512 MiB for initramfs.cpio and 8 GiB
for root.erofs. Root and head metadata are at most 16 KiB. Index partitions
must exactly cover each nonempty payload. Paths are compiled names, checked
decimal root versions, or 64-character lowercase hashes. Metadata cannot
supply filesystem paths or network locations.

Objects are named by SHA-256. Length and SHA-256 are checked on every cache
read and network acquisition. A bad cache entry is replaced only after a
verified download; missing or corrupt repository data fails. Object writes
use synced temporary files, rename, and parent-directory sync. Each object
is acquired completely before publication to the cache. Resume operates at
object granularity; a partial object of at most 256 KiB is fetched again.
There is no HTTP Range dependency. Response-body accounting includes metadata
and objects but excludes HTTP headers and TLS overhead. Request counts are
actual HTTP GET attempts, not inferred chunk counts.

HTTP redirects, ambient proxies, credentials in URLs, query strings and
fragments are refused. Plain HTTP is limited to numeric loopback addresses.
A reused HTTP agent permits keep-alive; requests are sequential. The absolute
acquisition budget is one hour, including individual DNS/request deadlines.
There is no automatic retry loop: rerun the command to resume cached objects.
Disk I/O and hashing do not acquire asynchronous interruption guarantees.

`fetch URL STATE CACHE OUTPUT` creates a new output directory. `OUTPUT.partial`
is reserved scratch and reclaimed on retry; its parent must be owned and
protected, and no other process may write there during acquisition. State
and cache leases serialize cooperating writers. The caller must provide safe
ancestors for state, cache, output and repository paths. This does not isolate
mutually hostile processes sharing a UID. Only one 256 KiB object, bounded
metadata and the index are needed in memory, rather than an entire image.
Cache collection is manual; it is safe to remove it while no acquisition is
active. There is no automatic cache-size quota.

The output contains the manifest and three reconstructed payloads plus
`head`, `release`, and up to 64 recently learned root documents. Verified roots are archived
beside STATE in `STATE.roots` before advancing state, so a failed acquisition
and retry retain their rotation proof. Provisioning at a newer root does
not reconstruct history before that root.
Publication requires all full-payload hashes and a final expiry recheck.
`verify STATE BUNDLE` performs offline signature, policy, freshness, replay
and complete-payload verification, persists state, and prints exactly
`COMMIT DEPLOYMENT_ID`. A privileged consumer must quiesce the producer and
protect its own state; a successful worker receipt alone is not authority.
`fetch` also retains received signed metadata in `STATE.metadata` before
advancing worker state. Root proofs precede the corresponding state writes.
`learn STATE EVIDENCE` independently authenticates those proofs without
requiring completed payloads. It ignores incomplete, forged or stale evidence,
but fails on protected-state parsing, clock rollback or persistence errors.
Valid root transitions persist even when later head/index data is invalid.
An expired but correctly signed head/index may raise version floors; future
issue times refuse. Learning expired evidence cannot authorize installation:
`verify` still requires current freshness and complete payload hashes. This
preserves observations across a crash that lasts beyond metadata expiry.
A privileged consumer must drain the worker before learning, learn after
failed/cancelled attempts, and learn retained evidence before overwriting a
worker snapshot on retry. It must never adopt unsigned worker counters.

An installer must still copy and verify the exact admitted deployment.
Neither command installs, changes boot selectors, or executes downloaded
code. Expiration governs admission, not boot or retained rollback.

## Partition comparison

`td-image-transfer partition cdc|hybrid DEPLOYMENT NEW_PARTITIONS` emits
three length plans. `publish-partitioned` takes the same arguments as
`publish`, followed by that directory. It accepts canonical positive decimal
lengths, one per line, at most 256 KiB each and exactly covering each payload.
It computes all hashes from the source bytes, verifies the deployment manifest,
and emits the same signed index as the default publisher. The receiver has
no EROFS parser or method flag: it verifies ordered objects and whole payloads.
This permits actual HTTP/cache comparison with identical transport semantics.
The experimental hybrid plan uses EROFS physical extents for root.erofs and
CDC for boot payloads. The default remains CDC.

## Publishing and retention

Commands use positional arguments; invoking `td-net release` prints syntax.
Times are Unix seconds and keys use ring's Ed25519 PKCS#8 private form and
lowercase hexadecimal public form. `keygen PRIVATE PUBLIC` exclusively
creates a private mode-0600 file and its public companion. Never put a
private key inside the served repository.

`root VERSION EXPIRES ROOT_PRIVATE RELEASE_PUBLIC PREVIOUS_ROOT_PRIVATE OUT`
creates a root document. For version 1, use ROOT_PRIVATE in both private-key
positions. For rotations, retain every intermediate document. `publish`
validates an existing deployment, emits immutable objects and a signed index,
and prints its index hash. It installs the supplied root document into
`roots/VERSION`, requiring the predecessor for versions above 1. Publication
holds a repository lease. Existing immutable files must match exactly.

`promote` verifies every referenced object before atomically replacing
`heads/PROFILE`. It requires a strictly newer head version, a nondecreasing
release sequence, and no same-sequence/different-index substitution. Its head
must expire no later than its image index. Renew freshness by promoting the
same index with a higher head version and a new bounded lifetime. An expired
image index needs a newly signed index and higher release sequence. Root
rotation requires re-signing under the new root; that authenticated epoch
starts fresh head and release counters. All payload objects precede metadata discovery. Repository and
cache files are private to their owner by default; hosting permissions are
an explicit operator setup outside this local prototype.

`gc REPOSITORY ROOT` is a dry run; `--delete` performs deletion under the
same lease as publication. It retains the heads' selected indexes even when
expired and all indexes until seven days after expiry. It computes object
reachability from that retained set. An unreachable object must also be at
least seven days old before deletion. Roots are never collected. The grace
exceeds the client's one-hour acquisition budget; a longer suspended client
can need a newer release. Missing selected indexes and corrupt metadata refuse collection rather than
guessing reachability. Installed current/previous deployments are outside
this repository collection entirely.

Local tests cover signed round trips, corruption, missing objects, an actual
truncated loopback HTTP response and resumed acquisition, metadata-only warm
downloads, an injected ENOSPC writer, replay, equivocation, time and profile
checks, sequential revocation and conservative object collection. These are
delivery tests; they do not claim an installed-machine or boot oracle.

## Boot oracle

`qemu-boot-system` and `qemu-boot-server` publish their transaction candidates with ephemeral fixture
keys, acquires them from a local static repository, and independently verifies
them with a separate state before staging the guest installation offer.
Each existing failed-health-boot rollback case then exercises an acquired
candidate. The headless profile exercises this without the desktop oracle's
Firefox readiness/input dependency. This covers signed acquisition through installation and rollback;
it does not exercise the SSH administration service or HTTP inside the VM.
The network tests separately cover loopback HTTP and interrupted transfers.

`qemu-admin-update` additionally boots the desktop deployment normally, with
an oracle-owned public repository and initial publisher/admin-key policy on
its disposable volume. A target-built test client runs as the primary user,
serves that repository on guest loopback, and invokes the installed restricted
SSH admin interface. It requires expired metadata and a truncated response
to leave selectors unchanged, a retry to reuse cached bytes and install,
and a rotated root to reject replay of the earlier root epoch. It then requires the
downloaded deployment to boot healthy, a subsequent default update to reboot,
and three failed candidate boots to fall back to the retained healthy image.
The test client and ephemeral private keys never enter a published image.
This is loopback HTTP and SSH inside the VM; public HTTPS remains a separate
hosting test. Initial trust is staged as trusted-media provisioning would
stage it, rather than exercised through the physical enrollment UI.
