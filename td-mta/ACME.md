# Native certificate issuance and renewal

The `td-mta acme` command is a dedicated native worker in the td-mta executable.
It uses the existing crypto provider and PostgreSQL; Certbot, OpenSSL, a shell,
and additional Rust dependencies are not runtime requirements. The first
supported deployment is one HTTP-01 worker owning one public endpoint and one
certificate profile. It can request up to 32 exact DNS names. DNS-01, wildcard
identifiers, external account binding, private CA directories and distributed
HTTP-01 routing are not supported by this command.

The host installer can generate systemd and Shepherd definitions for this
worker; see [DEPLOYMENT.md](DEPLOYMENT.md#native-certificate-service).
Start the worker before importing a managed mail configuration:

```text
td-mta acme --connection-file /private/runtime-connection \
  --certificate-profile public \
  --directory https://acme-v02.api.letsencrypt.org/directory \
  --contact operator@example.org --name mx.example.org \
  --bind 0.0.0.0:80 --accept-terms
```

The connection file is the protected runtime specification. Run under the
service UID, with permission to bind port 80. Supervise this command like the
mail service; it stays running between renewals. Only its HTTP-01 responder
binds port 80. The names must resolve to this endpoint and public port 80 must
reach it. Do not run a second challenge service on that address. Different
certificate profiles require independently reachable listener addresses.

The worker establishes its own account and certificate key in PostgreSQL before
requesting issuance. Initial issuance does not require an existing certificate
or selected mail configuration. Mail remains unavailable until a valid managed
pair can be loaded. There is no self-signed fallback. Once issuance succeeds,
import/select the mail profiles with `mode = "acme"` and the same certificate
profile label, directory, contact and exact DNS-name set. Name order is ignored
when matching this identity. An HTTP-01 listener declaration remains required
in those profiles; the separate native worker owns that listener. Explicit
operator-provisioned certificate material remains supported as `mode = "files"`.

Inspect the worker without exposing keys, order URLs or challenge tokens:

```text
td-mta acme-status --connection-file /private/runtime-connection \
  --certificate-profile public
```

Status reports pending, ready, degraded or expired, the selected certificate
generation, expiry, next attempt, failure count and whether an owner lease is
live. Ready is a database observation, not an external reachability test.
By default a successful status query exits zero even when the worker is not
ready. Add `--require-ready` for supervisor health checks: it exits one unless
the status is ready. `--minimum-validity-seconds N` additionally reports
`expiring` when the active certificate has less than N seconds remaining;
N is 0 through 31536000, default zero. Combine both options to fail a check
before expiry, for example `--require-ready --minimum-validity-seconds 2592000`
for a 30-day margin. A live lease and no recorded renewal failures are also
required for ready status. These checks do not trigger issuance or renewal.

Certificate success/failure events use the bounded application-log tables.
Database outages can prevent both persistence and logging; the HTTP-01 cache
expires and the worker retries ownership acquisition. No peer diagnostics or
private keys enter logs. SIGINT/SIGTERM stop the worker after its bounded active
turn and close the challenge listener.

## Bounds and recovery

Schema 30 adds at most sixteen profile rows. Each contains at most 32 KiB of
private pending state, 64 KiB of active chain and 1 KiB of active key. The state
includes account and certificate keys, account/order URLs, one current challenge,
retry time, issuance-attempt time and certificate expiry. These bounds describe
live data; PostgreSQL WAL, MVCC and whole-volume headroom remain operational
responsibilities.

Every ownership claim and state update checks the store epoch and forces
synchronous commit regardless of the database role default. Claims use a
180-second database lease, a random process owner and an increasing revision.
Each network turn is capped at 100 seconds, with per-exchange 15-second caps,
shorter DNS/connect caps and bounded database operations. No SQL transaction
remains open during network I/O. A stale worker cannot save or select a pair.
Every turn reloads durable state, including after an uncertain publication.
Failure backoff is saved only after reclaiming and reading that durable outcome.
Account registration reuses the stored account key. Interrupted orders reuse
the stored certificate key and URL. An uncertain newOrder response, before its
URL is retained, permits no new order for a day; it can leave an orphan order
at the CA but cannot cause a rapid issuance loop.

CA endpoints must remain on the configured HTTPS origin, preserving their
exact URL spelling in signed requests. Redirects are refused. Responses have
16 KiB header, 64 KiB decoded-body and 128 KiB total-wire ceilings. Framing
supports Content-Length, simple chunked encoding and authenticated TLS EOF;
ambiguous lengths, encodings and duplicate critical headers are refused.
HTTP-01 serves two concurrent requests with a 4 KiB request limit and a
five-second connection lifetime. Each peer occupies at most one slot. When
both are occupied a different peer can replace the oldest connection idle
for at least a second. The responder retries temporary accept failures,
including descriptor exhaustion, between service turns. It returns only the exact current
host/token authorization while the local lease cache remains fresh. Multiple
names are authorized sequentially; an answer remains available until its
authorization is valid.

Before publication, the provider validates key/chain consistency, validity and
exact DNS SAN identifiers. This local identity check is not an independent
public-root proof; the certificate comes from the authenticated configured CA.
The active chain, key, generation and pending state change in one fenced update.
A reader pins the entire pair once per certificate profile, so concurrent
renewal cannot mix a new chain with an old key.

Renewal starts around half of the actual certificate lifetime, with stable
jitter up to five percent of that lifetime. Failures use persisted exponential
backoff up to six hours; CA retry instructions are bounded to one day. A failed
renewal preserves the active pair. A delayed completed order is published while
still valid and scheduled for immediate renewal if already due; an expired
completed order is retired so a new order can proceed. Expired certificates do not authorize new
protected mail service.

Managed mail instances check their certificate revisions every thirty seconds.
On a change, the watcher prepares the selected profile's inbound TLS policies
with the same identity, key, validity and hostname checks as startup. All
loaded managed pairs must match the observed revisions and the database
selection must still match the running profile. A single publication makes
the new certificates available to fresh HTTPS and SMTP STARTTLS handshakes.
The listeners stay bound and established connections finish on their original
TLS sessions. Device revocation checks continue on established JMAP streams.

The profile has room for its current serving certificates and one candidate
or retired generation. When a session still pins a retired generation, the
watcher retries on the next observation. A failed refresh retains the
current certificates subject to their original validity checks. A serving
process emits `certificate_renewal_failed` when a changed revision fails
activation. Capacity waits and superseded observations are retried quietly. A general
configuration selection change still causes a bounded drain and reload.

## Qualification

The offline protocol fixture exercises badNonce, exact request signing inputs,
HTTP-01 publication before acknowledgment, finalization, certificate validation,
restart at every retained checkpoint and uncertain newOrder pacing. A local CA
fixture runs the complete issuance flow over verified TLS and fetches the actual
HTTP-01 endpoint. Native PostgreSQL tests cover upgrade, ownership conflicts,
stale revisions, lease takeover, invalid-pair refusal, asynchronous role defaults
and the sixteen-row cap. Recovery fixtures cover uncertain publication, delayed
authorization across multiple names and late retrieval of completed orders.
Native mail process tests publish a new leaf and issuer, verify fresh HTTPS
and SMTP STARTTLS connections against that issuer, then observe SMTP delivery
and device revocation on the JMAP stream opened before publication.
Live staging/public-CA acceptance is deployment validation, not part of the gate.
