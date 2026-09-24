# Complete configuration schema

## Status and ownership

This document specifies the complete v1 operator schema to implement in
M04b2c3. Typed statement dispatch now closes structural references, but
owned whole-reader loading and a runnable service remain unimplemented. CONFIG.md owns physical syntax, resource fields, local routing and
stream completion. API.md owns visible identity encoding. This document owns
the remaining fields, cross-references and candidate construction. Protected
file loading and redacted effective output remain M04b3/M05; M19 owns
publication.

No ordinary configuration load contacts DNS, a relay, a CA or a listening
socket. Structural acceptance does not prove file permissions, certificate
validity, DNS ownership or reachability. A successful public `config check`
requires the later protected-file and provider checks as well.

## 1. File structure and common values

The first nonempty statement is exactly `version = 1`, once. No other root
assignments exist. Remaining stanzas may appear in any order. Resolve
references only after the entire reader reaches EOF. Reject unknown
sections/fields, wrong labels/types, duplicate singleton sections, repeated
fields, missing required fields, conflicting declarations and dangling
references. No default silently repairs a supplied invalid value. Source
errors use fixed codes, static field names and physical locations; they never
echo input bytes.

| Value class | Rules |
| --- | --- |
| Object ID | Exactly 32 lowercase hex digits; typed by its role |
| Profile name | 1..64 ASCII bytes matching `[a-z][a-z0-9_-]*`; case-sensitive |
| DNS name | ASCII labels of 1..63 bytes, letters/digits/interior hyphens; no empty labels, trailing dot, address literal, underscore or all-digit final label; fold domain case |
| Server/listener/relay/MX/URI-authority DNS name | At most 243 bytes, matching the routing-domain bound |
| Derived certificate DNS name | At most 253 bytes; includes `mta-sts.` plus a served domain |
| IP address | Numeric IPv4 or IPv6; no zone ID or hostname lookup |
| Bind address | Numeric socket address with shortest unsigned decimal port 1..65535; IPv6 uses brackets |
| Absolute path | 1..4095 UTF-8 bytes; leading `/`; no NUL, empty interior component, `.` or `..`; trailing `/` only for root `/` |
| Boolean/integer | CONFIG.md literal grammar, with field ranges below |

DNS names are operator-supplied ASCII names, including any required A-labels;
there is no implicit Unicode IDNA conversion. Lexical path checks do not
resolve symlinks or establish trusted ancestors. Settings never derive mail
paths from addresses, display names or arbitrary client IDs.

A JMAP origin is an HTTPS origin with a DNS authority, optional port and no
userinfo, query or fragment. An absent port means 443. An optional terminal
slash means the same origin. Canonical output folds the hostname, omits port
443 and omits the trailing slash. Scheme spelling is lowercase `https`. URI
ports are 1..65535 in shortest unsigned decimal spelling: reject empty, zero,
leading-zero or signed ports. JMAP origin text is at most 4096 bytes;
URI-authority DNS names use the 243-byte bound above. These
authority/port/scheme rules also apply to the ACME URI. URI authorities also
reject a final label consisting of `0x`/`0X` and zero or more hex digits: URL
clients can reinterpret those as numeric IPv4 components. This is an HTTPS
authority restriction, not a change to SMTP routing DNS names.
`config::endpoint` enforces it for URI/origin helpers. The
[URL host parsing rules](https://url.spec.whatwg.org/#ends-in-a-number) motivate
this stricter authority profile.

An ACME directory URI also uses HTTPS and a DNS authority, but may have a path
and query. Its ASCII path/query must follow RFC 3986 character and
percent-triplet syntax; reject fragments, userinfo, backslashes, spaces and
controls. Literal and percent-encoded dot segments remain verbatim; no path
authorization is inferred from them. An empty path is `/` for HTTP requests.
Preserve the validated URI's
spelling for the consumer, separately from its folded TLS hostname and checked
numeric port. JWS endpoint handling remains M18 and must not reinterpret URI
identity. [URI syntax](https://datatracker.ietf.org/doc/html/rfc3986) supplies
the character grammar; the DNS-only authority and byte ceilings are this
service's profile.

## 2. Global settings

Singleton sections have no label. Unless marked required, a section may be
omitted when all its fields have defaults.

### `[server]` — required

| Field | Type/default | Constraint/meaning |
| --- | --- | --- |
| `hostname` | DNS text, required | Outbound EHLO and loopback-fixture greeting identity |
| `jmap_origin` | Origin text, required | Sole JMAP discovery/HTTP authority; never derived from Host |
| `online_background` | Boolean, true | Select OnlineBackground or ForegroundOnly resource planning |
| `public_ipv4` | IP text, absent | Optional IPv4 address for `dns-plan`; never infer it from a wildcard/private bind |
| `public_ipv6` | IP text, absent | Optional IPv6 address for `dns-plan` |

Public address hints must be numeric values of the stated family. Reject
unspecified/multicast values, IPv4 limited broadcast, IPv4-mapped IPv6 and
obsolete IPv4-compatible IPv6; retain IPv6 loopback ::1. Private and loopback
addresses are allowed for explicit fixtures. These checks do not prove
public routing or DNS ownership.

The public address fields do not bind sockets or configure interfaces. Missing
addresses make `dns-plan` report missing address inputs, not invent records or
perform a network lookup. Outbound source-address selection remains the OS's
routing decision; it is not controlled by these publication hints.

### `[paths]`

| Field | Default |
| --- | --- |
| `data` | `/var/lib/td-mta` |
| `runtime` | `/run/td-mta` |
| `logs` | `/var/log/td-mta` |

All three are absolute paths. Reject equal or component-wise nested roots.
Compare whole slash-delimited components: `/a/b` nests under `/a`, but
`/a-other` does not; root `/` contains every other absolute path. M05
additionally checks resolved descriptor identities and trust. The CLI's
explicit configuration path selects the operator file, normally under
`/etc/td-mta`; there is no redundant `config_root` field. Secret/signature/key
references are absolute and do not expand variables or use implicit search
paths.

### `[resolver "NAME"]` — 1..4 required

Each resolver has exactly one required `address` field, a numeric socket
address with shortest unsigned decimal port 1..65535. It identifies the
UDP/TCP DNS server; it is not a bind. Reject duplicate numeric endpoints,
mapped IPv6, zone IDs, unspecified addresses, multicast, the IPv4 limited
broadcast address and obsolete IPv4-compatible IPv6 addresses. IPv6 loopback
`::1` remains valid. These are destination rules; generic bind addresses and
gateway CIDR ranges have their own rules. Resolver stanza order sets fallback
order. V1 uses these explicit endpoints only, never
ambient NSS, `/etc/resolv.conf` or automatically selected public resolvers. A
loopback resolver requires an actual service at that endpoint; configuration
does not provide one. Tests name their local fixture address explicitly.
Resolver changes require restart. M09 uses bounded endpoint-index/generation
cache keys for the relay and ACME directory hostname; it cannot cache
arbitrary incoming domains. An offline migration job may temporarily own one
additional bounded destination slot, validated by its explicit source-origin
input, and releases it on exit.

### `[logging]`

`minimum_severity` is text `info` (default), `warning` or `error`, using the
existing event severities. Rolling size and retention remain `[limits]`
fields. No raw protocol, credential or message-content logging mode is added.

The existing `[limits]`, `[disk]`, `[work]` and `[network]` singletons retain
CONFIG.md's exact schema. Finalization selects the server view mode and runs
all three resource planners before a structural candidate can be returned.

## 3. Account, domains and aliases

Exactly one `[account "ID"]` is required. Its fields are:

| Field | Type/default | Constraint |
| --- | --- | --- |
| `username` | Text, required | 1..254 visible ASCII bytes, excluding colon; exact case-sensitive HTTP Basic username |
| `name` | Text, empty | At most 4096 UTF-8 bytes; configured account display name |

Device application-password records remain service-managed `devices/` state.
There are no password/token/verifier fields in the account stanza. Changing
the account ID is an offline migration/reinitialization operation, not a
reload or a way to reinterpret an existing account's files.

There are 1..256 `[domain "DNS-NAME"]` stanzas and at most 4096 `[alias
"SMTP-ADDRESS"]` stanzas. Their account/domain, case, quoted-address,
postmaster and duplicate rules remain CONFIG.md's routing contract. Each alias
has exactly one required `account = "ID"` field and no others.

A domain additionally has these policy fields:

| Field | Type/default | Constraint/meaning |
| --- | --- | --- |
| `mx_host` | DNS text, server hostname | Actual published MX target, including an upstream gateway when applicable |
| `mx_preference` | Integer, 10 | 0..65535; one published MX target per domain in this profile |
| `mta_sts` | Text, `off` | `off`, `testing`, `enforce` or `none` |
| `mta_sts_max_age_seconds` | Integer, 86400 | 0..31557600 |
| `mta_sts_certificate` | Profile name, absent | Required exactly when policy publication is enabled |

If no `direct_smtp` listener exists, structural validation requires an
explicit `mx_host` for every domain, including gateway-only or loopback-only
fixtures. The host receiving from a gateway is not implicitly its public MX. A
defaulted MX must match a direct listener's `server_name`. If an explicit MX
equals any name advertised by this host (global hostname, direct/gateway
listener server name, JMAP origin host or any enabled MTA-STS policy host),
it also requires a matching direct listener name/certificate requirement.
A gateway-only or HTTPS-only name cannot become a public MX by spelling it
explicitly. Other explicit MX names designate an upstream endpoint whose
certificates this host cannot validate offline. This classification controls DNS output; it is not a claim about
actual DNS routing. `dns-plan` emits the configured MX record; it emits this
host's address hints only for its own advertised listener/origin/policy names,
never an unrelated gateway MX.

Enabled policy publication serves the domain's exact MX name and configured
mode/lifetime from `https://mta-sts.DOMAIN/.well-known/mta-sts.txt`. Require
an HTTPS listener on port 443 and the named certificate covering that policy
host. `none` publishes a disabling policy; `off` publishes no policy. Removing
DNS publication does not clear a sender's cached enforcement, so these are
separate states. Mode, age bounds and the policy host follow [RFC 8461
§3](https://datatracker.ietf.org/doc/html/rfc8461#section-3). This is static
policy publication, not remote policy discovery or DNS editing. The deployment
recommendation is to begin with testing and validate DNS/TLS before selecting
enforce; offline configuration cannot prove that readiness. When mode is
`off`, forbid `mta_sts_certificate`; a supplied max age is still range-checked
and retained but has no published effect until policy is enabled.

The canonical policy bytes are four ASCII lines in this order, each ending in
LF: `version: STSv1`, `mode: MODE`, `mx: LOWERCASE-MX-HOST`, and `max_age:
SECONDS`. Integers use shortest decimal spelling. This profile emits the MX
line even for `none`. M18 derives the TXT `id` as the lowercase hex of the
first 16 bytes of SHA-256 over those exact bytes (32 characters). The provider
computes the digest; no separate operator ID setting or full policy buffer is
needed. Identical policy bytes retain the ID across reload/restart; changed
policy bytes derive a new ID. The ID is a cache version, not an authentication
proof. `dns-plan` emits `v=STSv1; id=ID;` only for enabled publication.
Publish the matching HTTPS policy before updating its DNS TXT record; tools
report this required order and never edit DNS. Mode `off` emits neither a
policy nor a TXT record. M18 tests literal body/ID fixtures, unchanged IDs and
changes to every policy field before enabling these outputs.

## 4. Sending identities

Require 1..64 `[identity "ID"]` stanzas. Every identity belongs to the sole
account and explicitly authorizes its configured From/envelope address under
QUEUE.md. Inbound aliases do not create identities; an identity address need
not be an inbound alias. Provider acceptance is still the relay's decision.
Wildcard identities are unsupported: reject an identity email whose decoded
local part is exactly `*`, including quoted spellings. Other address-valued
fields retain their mailbox syntax; this restriction belongs to identity
sending authority.

| Field | Type/default | Constraint |
| --- | --- | --- |
| `account` | Object ID text, required | Sole account ID |
| `name` | Text, empty | At most 4096 UTF-8 bytes |
| `email` | SMTP mailbox text, required | ASCII mailbox spelling and 254-byte address/64-byte local/243-byte domain limits from CONFIG.md; no domainless form |
| `reply_to` | Boolean, false | false means null; true means a present ordered list |
| `bcc` | Boolean, false | false means null; true means a present ordered list |
| `text_signature_file` | Absolute path, absent | Absent materializes an empty signature |
| `html_signature_file` | Absolute path, absent | Absent materializes an empty signature |

The boolean list selectors are operator syntax, not the JMAP wire types.
Repeated `[identity_address "IDENTITY-ID"]` stanzas supply list entries:

| Field | Type/default | Constraint |
| --- | --- | --- |
| `kind` | Text, required | `reply_to` or `bcc` |
| `name` | Text, absent | Null when absent; supplied empty text remains an empty name |
| `email` | SMTP mailbox text, required | Same mailbox representation limits as identity email |

Address stanzas may precede their identity. Preserve physical declaration
order within each identity/list; do not deduplicate or sort visible addresses.
Reject rows whose list selector is false. True with no rows materializes an
empty array. Each list has at most 16 entries, with at most 2048 total address
rows. Repeated address stanzas are list entries, not duplicate singleton
sections; repeated fields inside any one stanza remain errors.

Signature references name bounded UTF-8 files, at most 16384 bytes each after
reading. Preserve newlines and text bytes; reject NUL and invalid UTF-8. They
are data, not includes, templates or hooks. Loading them requires M04b3/M05's
protected operator-file validation. Other inline text fields retain the
parser's 4096-byte bound. Signatures have file references only; there is no
inline signature field or multiline syntax.

Materialize every visible property, including null/empty distinctions and
`mayDelete=false`, before the API.md encoder runs in the protected-input
finalization stage. Preserve configured visible strings; comparisons for
From/envelope authorization use mailbox semantics separately. Sender
authorization preserves decoded local-part case and folds only the domain; the
inbound routing-only postmaster exception does not widen sending authority.
Canonical identity order is ascending raw ID. Duplicate IDs and unknown
account/identity targets refuse the candidate. The total preimage and all
non-routing text must independently fit their existing bounds: identity
strings, other settings and decoded credentials together fit the 192 KiB text
arena, while the streamed preimage has its separate 192 KiB encoded-byte
limit. There is no retained preimage allocation. Field defaults mean the
values in the tables above; RFC 8621 Identity has no `isDefault` property or
required server-selected default identity.

## 5. Smart host

Exactly one unlabelled `[relay]` section is required. There is no recipient-MX
fallback or plaintext mode.

| Field | Type/default | Constraint |
| --- | --- | --- |
| `host` | DNS text, required | TLS verification name and DNS resolution target |
| `port` | Integer, required | 1..65535 |
| `transport` | Text, `implicit_tls` | `implicit_tls` or `required_starttls` |
| `username` | Text, required | 1..254 printable ASCII bytes; no controls |
| `password_file` | Absolute path, required | Protected credential input; never inline |
| `ca_file` | Absolute path, absent | Protected explicit trust override; absent uses reviewed public roots |

The initial provider settings are `smtp.migadu.com`, 465, `implicit_tls`.
Tests use local fixtures only. The selected transport verifies chain, validity
and configured hostname before authentication, without a downgrade or
verification-disable option. AUTH mechanism selection follows M17's advertised
PLAIN/LOGIN support, not a configuration guess about provider software.

M04b3 must define bounded password-file decoding before it claims resolved
credentials; the trust roles and material ceilings below already apply. The
schema's reference is not the password, and its structural acceptance grants
no authority to send an AUTH command.

## 6. Certificates and ACME

Use 1..16 `[certificate "NAME"]` profiles:

| Field | Type/default | Constraint |
| --- | --- | --- |
| `mode` | Text, required | `acme` or `files` |
| `chain_file` | Absolute path, absent | Required for files mode; forbidden for ACME |
| `key_file` | Absolute path, absent | Required for files mode; forbidden for ACME |

Every profile must have a configured consumer. Derive identifier sets from
listener server names, JMAP origin and enabled MTA-STS hosts, with at most 32
distinct names per profile and 512 bindings overall. Repeated use of a name
within one profile does not duplicate an identifier. The current schema
produces at most 272 distinct bindings (16 listeners plus 256 domains).
The 512-cell reservation remains the conservative 16-profile by 32-name
ceiling; it does not claim all 512 entries are reachable. Derived-name
text, including copies of existing consumer names, is charged to the
shared non-routing arena. A name cannot map to
conflicting certificate profiles in the same HTTPS SNI routing table.

Files mode has explicit operator renewal responsibility. ACME mode requires an
unlabelled `[acme]` singleton with these fields:

| Field | Type/default | Constraint |
| --- | --- | --- |
| `directory` | HTTPS URI text, required | At most 4096 ASCII bytes |
| `contact` | SMTP mailbox text, required | Same 254/64/243-byte mailbox limits; encoded as a mailto contact by M18 |
| `terms_accepted` | Boolean, false | Must be true to enable orders |
| `ca_file` | Absolute path, absent | Protected trust override for the directory HTTPS connection |

Reject an ACME section with no ACME profile. Reject `terms_accepted = false`
(including its absent default) structurally; no pending issuance state is
entered for unaccepted terms. Require an HTTP-01 listener on port 80 for ACME
profiles. Private gateway names normally use files mode; ACME consumers
require operator-provided DNS and reachable HTTP-01 validation. Offline
structural checks cannot establish that reachability or CA willingness to
issue. M18 surfaces CA-required account/terms failures without silently
changing the operator profile. Operational directory/order/authorization/
challenge/certificate URLs must retain the configured directory's HTTPS origin
(folded host and port); cross-origin endpoints and redirects fail explicitly
in v1. Terms links are displayed, never fetched automatically. This bounds
resolver identities and trust routing; not every CA topology is supported. To
construct contact URIs, prepend `mailto:` and retain ASCII unreserved
characters plus the single mailbox separator `@`; percent-encode every other
mailbox byte using uppercase hex, including quoted-local delimiters, spaces,
`?`, `#` and `%`. Quoted-local embedded `@` bytes are encoded as `%40`, not
treated as separators.

ACME account keys, orders and renewed certificates are service-managed state,
not additional operator include files. Bound provider-parsed material in the
separate certificate-generation ledger; do not keep an uncharged second copy
inside configuration text. M07/M18 own key/chain matching,
algorithm/expiry/name validation, returned identifier checks and atomic
renewal. No certificate is trusted merely because its profile reference
resolves.

Bound each raw chain file/response to 64 KiB, private key to 16 KiB, and
explicit CA bundle to 128 KiB. These are individual ceilings, not simultaneous
allocation promises. One certificate generation, including all profiles,
relay/ACME/gateway trust stores, raw material still retained, parsed provider
objects and allocator overhead, must fit its existing 1 MiB reservation.
Public-root parsed objects are charged here too; immutable compiled root bytes
belong to process/image allowance. Reject combined overflow even when every
file meets its own bound. Renew one complete generation at a time: retain at
most old and replacement 1 MiB generations, with no per-profile side
generations or uncharged trust cache. M03/M07 must prove provider allocation
bounds and old/new/session overlap before enabling material loading; bounded
input alone is not that proof.

## 7. Gateway policies

There are at most 16 `[gateway "NAME"]` policies. Each network gateway policy
has required `ca_file` and `client_cert_sha256` fields and optional
`next_client_cert_sha256`. Paths are absolute; each pin is exactly 64
lowercase hex digits encoding SHA-256 of client leaf DER. Two pins permit
explicit rotation overlap and must differ. Both still require the configured
private chain trust, validity and client-auth usage; a pin is not a
verification bypass.

Repeated `[gateway_peer "GATEWAY-NAME"]` stanzas each contain one required
`network` text field: a canonical IPv4/IPv6 CIDR prefix with zero host bits.
IPv4 uses four shortest decimal octets; IPv6 accepts valid numeric hex forms
and compares their binary addresses, so equivalent compressed/uncompressed
spellings are the same prefix. Prefix lengths use shortest unsigned decimal.
Allow 1..32 prefix bits for IPv4 and 1..128 for IPv6, at most 8 prefixes per
policy and 128 overall. Refuse mapped-IPv6 prefix spellings and compare mapped
socket peers as IPv4 for admission; stored receipt peers still preserve the
original socket form, as FORMAT.md requires. Overlapping prefixes within one
policy are redundant but not an identity conflict; duplicate exact prefixes
are errors. Each used network gateway policy requires at least one prefix.
Unused gateway policies are permitted as bounded staged configuration. Their
fields and supplied peer rows must still validate; a peer referencing a
missing gateway is always an error. A declaration without a consumer is not a
dangling reference. Unused certificate profiles remain forbidden under §6.

A gateway listener selects exactly one policy, so its verified pins and
allowed peers map unambiguously to that gateway name. Firewall restriction is
also an operator deployment requirement. These policies never authorize
nonlocal recipients or trust sender-authentication headers. No proxy protocol
is enabled.

The plaintext loopback fixture role below uses no gateway policy, CA or pin.
It is explicitly marked as fixture ingress and cannot produce a verified
Gateway TLS identity. It still obeys local-recipient and resource limits.

## 8. Listener roles

Require 1..16 `[listener "NAME"]` stanzas with required `kind` and `bind`.
Kinds are `direct_smtp`, `gateway_smtp`, `https`, `http01` and
`loopback_smtp_fixture`. At least one SMTP role and one HTTPS listener are
required. No SMTP submission, implicit inbound submission, IMAP or POP role
exists in this schema.

| Field | Applicability |
| --- | --- |
| `server_name` | DNS text; required for direct/gateway SMTP, forbidden otherwise |
| `certificate` | Profile name; required for direct/gateway SMTP and HTTPS, forbidden otherwise |
| `gateway` | Profile name; required for network gateway SMTP, forbidden otherwise |
| `session_limit` | Integer 1..smtp_sessions; required for SMTP roles, forbidden otherwise |
| `per_peer_limit` | Integer 1..session_limit and at most smtp_per_peer; required for SMTP roles, forbidden otherwise |

Direct/gateway listeners emit their own `server_name` in the SMTP greeting and
EHLO response; it may differ from the global hostname. It also supplies that
listener's required certificate name. Loopback fixtures use the global
hostname. HTTP-01 listeners bind port 80 only, including local fixtures.

Direct SMTP offers opportunistic STARTTLS under DESIGN.md's compatibility
policy. Gateway SMTP requires STARTTLS and verified client identity plus peer
admission. A failed TLS upgrade closes the connection. The fixture role
accepts plaintext only on an explicit loopback bind (IPv4 127.0.0.0/8 or IPv6
::1) and never gains gateway trust. Selecting the direct role selects
plaintext compatibility in v1; no additional direct plaintext-toggle field
exists. HTTP-01 serves challenges and the fixed-origin redirect only; it never
accepts credentials or JMAP writes. HTTPS serves the one configured JMAP
authority and configured MTA-STS names, with checked SNI/certificate
selection.

Every HTTPS listener's primary certificate covers the JMAP origin hostname,
and its port matches that origin. Enabling MTA-STS requires origin port 443;
the same HTTPS binds then serve those policy hosts with each domain's selected
certificate profile. Unknown authorities are refused. This profile does not
create a separate policy-only HTTPS port.

Global pool/handshake/per-peer limits still apply. Sum SMTP session limits
across all SMTP roles and require that sum not exceed smtp_sessions; direct
and gateway roles retain their own counters and policies. Loopback fixtures
also have separate per-listener counters within that same global reservation;
they have no extra slots and cannot borrow another listener's allocation.
HTTPS listeners share the bounded HTTPS pool without allocating one pool per
bind. HTTP-01 uses the separate control slots already specified in
RESOURCES.md.

Reject duplicate or overlapping binds of the same address family and port,
including wildcard overlap. IPv6 binds are IPv6-only in the target runtime,
with separate IPv4 binds when needed; M11 must implement and verify this
explicit socket policy before opening them, with M03/M07 adapter support. Safe
std alone does not establish IPV6_V6ONLY before bind: until an audited socket
adapter and its UNSAFE.md/component amendments exist, runtime startup refuses
IPv6 listeners. Structural acceptance of their requested layout never bypasses
that gate. It is not inferred from ambient kernel defaults and authorizes no
unreviewed syscall surface. Refuse mapped IPv6 bind spellings and
zone-qualified addresses.

## 9. Snapshot storage and construction

The existing 1 MiB snapshot remains 512 KiB text, 128 KiB aliases and 384 KiB
other metadata. Routing retains 320 KiB text and 4 KiB domain descriptors;
every other text value and decoded credential shares the remaining 192 KiB.
Fixed count ceilings do not promise that every combination of maximum-length
strings/files fits. Capacity refusal is a configuration error, never a reason
to allocate an overflow arena.

The target metadata partition is:

| Use | Ceiling |
| --- | ---: |
| Routing domain cells | 4 KiB |
| 64 identity cells, at most 128 bytes each | 8 KiB |
| 2048 identity address cells, at most 32 bytes each | 64 KiB |
| 256 domain policy cells, at most 64 bytes each | 16 KiB |
| 16 listener cells, at most 128 bytes each | 2 KiB |
| 16 certificate profile cells, at most 128 bytes each | 2 KiB |
| 512 certificate consumer/name bindings, at most 32 bytes each | 16 KiB |
| 16 gateway cells, at most 256 bytes each | 4 KiB |
| 128 peer-prefix cells, at most 32 bytes each | 4 KiB |
| Future non-authoritative device cache, 256 entries of at most 128 bytes | 32 KiB |
| Checked resource plans | 4 KiB |
| Global settings, references, indices, ownership and unassigned headroom | 228 KiB |
| **Total** | **384 KiB** |

These are implementation ceilings, not measured struct sizes. Every new layout
must have a size/capacity check before use. The persistent representation uses
private checked text references and indices, not a self-referential owning
Rust object. Temporary borrowed identity/address arrays live in an 80 KiB
reservation within the existing 256 KiB control-worker stack, outside the
snapshot owner. Their measured current host layouts total 72704 bytes; target
size checks remain mandatory. This leaves 176 KiB for all other control-worker
call frames, initialization copies and provider stack use. Do not place
another full view array on that stack. M04b3/M07/M23 must verify peak stack
usage before enabling this path. Stack arrays borrow the snapshot only within
finalization, do not escape, and are dropped before moving/publishing the
owning snapshot. No typed reinterpretation of byte arenas, self-reference or
extra allocation is required.

Resolver/relay records use at most 1 KiB of global settings/headroom,
including four inline resolver cells, relay fields and their text owner.
Their builder fits 1 KiB of the existing 36 KiB builder workspace. Moving
records into the candidate does not create another persistent copy. All
names, usernames and unresolved paths use the shared 192 KiB non-routing
text region.

Certificate profile cells use their separate 2 KiB partition. Their complete
borrowed records header, including the ACME singleton, fits 128 bytes of
global headroom, and the builder fits 256 bytes of builder workspace.
Listener cells use their separate 2 KiB partition; their builder and
borrowed records header each fit 128 bytes in the corresponding
workspace/headroom. Gateway cells and peer cells use their separate 4 KiB
partitions; reserve 128 bytes each for their builder and borrowed records
header in workspace/headroom. These gateway header reservations are enforced
by the combined layout check in M04b2c3d; current gateway host tests check
the builder size. Structural helpers perform no DNS lookup, protected-file
read or network operation.

Graph binding cells use the existing 16 KiB partition. The enclosing
borrowed graph header, including listener/certificate/gateway/domain
headers, fits 1 KiB of global headroom and replaces the individual headers.
Required names and canonical JMAP origin use shared non-routing text.
Binding uses one 253-byte name scratch and one 257-byte canonical-origin
scratch within the existing control-worker stack allowance. The whole
loader stages at most 258 raw origin bytes plus its source coordinate in
builder workspace until binding stores the canonical origin. Concrete
combined layout and peak stack checks remain required before publication.

Global options and lexical-root records fit 128 bytes of global headroom;
their builder fits 256 bytes of existing workspace. Numeric address hints,
flags and severity are inline, while roots use shared non-routing text.
Hostname and origin are staged separately for their existing single
canonical copies in domain-policy and graph records.

The loader owns one pending stanza within that same 36 KiB builder
workspace. Reserve 13 KiB for the largest pending variant, including text
and field bookkeeping. Reuse this region between stanzas; do not allocate
one buffer per section or add the per-variant footprints together. Maximum
retained text for the larger variants is:

| Pending stanza | Text bytes before bookkeeping |
| --- | ---: |
| Identity: label/account IDs, display name, email and two signature paths | 12604 |
| Paths: three root paths | 12285 |
| Relay: host, username, transport and two paths | 8704 |
| ACME: directory, contact and CA path | 8445 |
| Files certificate: label, mode and two paths | 8259 |
| Identity address: label, kind, display name and email | 4390 |
| Account: label, username and display name | 4382 |
| Gateway: label, CA path and two hex pins | 4287 |
| Server: hostname, raw origin and two address hints | 561 |
| Domain: label, MX host, policy mode and certificate name | 557 |
| Gateway listener: label, kind, server name, two profile names and bind | 500 |

`config::stanza::Pending` implements this shared pending region with 12672
text bytes, eight typed field cells, source coordinates and one label span.
The complete representation is compile-time bounded to 13 KiB. Text counts
above include raw IDs and enum spellings until typed handoff; scalar integers
and booleans are inline. Resource stanzas use the existing resource builder
and do not accumulate another pending field array. This helper stages only
one non-resource stanza and grants no schema, EOF or publication authority.

Relay and certificate/ACME variants individually fit within 9 KiB including
bookkeeping; the shared 13 KiB reservation also accommodates identity and
root-path variants. Decoded protected-file contents are loaded later into
the candidate's text/material budgets, never into this pending
operator-stanza buffer. M04b2c3d must measure the complete pending
representation and all concurrent builder/header/global staging state
against the combined 36 KiB ceiling before use. This partition does not
increase the existing 64 KiB parser scratch reservation.

Retain only one alias label (at most 254 bytes) until its account field is
known, then hand it to the routing builder; never duplicate all alias
spellings into non-routing text. The resource builder uses its existing 4
KiB allowance within the same workspace. Static whole-loader fields such as
the staged server hostname also count against that workspace, outside the
reusable pending variant.

The public whole-loader entry point owns the candidate and stream operation as
one call. It must not accept an unrelated Summary as EOF evidence. On any
read, syntax, schema or reference failure, drop candidate authority and retain
no publishable partial result. Reusable private backing bytes may remain.
After EOF, resolve references and validate routing/resource plans. Return a
structural candidate whose parsed settings are immutable, with private unused
text capacity reserved for finalization. It is not a completed snapshot. M04b3
consumes that candidate through M05 protected reads, appends signatures and
decoded credentials into its existing 192 KiB non-routing region, and refuses
any read/UTF-8/permission/capacity error without returning an authority. It
then builds the temporary borrowed views and invokes the existing encoder. M07
validates provider material. Only successful completion seals the immutable
publishable snapshot for M19; there is no mutation of a sealed active
snapshot. M04b2c3b1 tests unresolved signature references/defaults; M04b3 uses
bounded injected protected-input fixtures to test materialization and
encoding.

Initial ACME issuance uses DESIGN.md §7's bootstrap state: after structural,
resource and protected-input validation, M19 may expose only HTTP-01 and local
diagnostics while obtaining missing certificates. It must report pending
issuance explicitly, without accepting mail or exposing JMAP. On reload,
retain the old application generation until replacement certificates are
validated; challenge work for the pending candidate does not publish its
application policy. A reload may hold one pending candidate for at most 60
seconds monotonic time, yielding control between bounded steps; the command
reports `reload_pending` with a local operation ID, then a terminal result.
Timeout returns `certificate_pending`, cancels local challenge work and
releases the candidate; resumable managed order data may remain on disk. A
newer reload supersedes the pending candidate, cancels its challenge work and
releases it before allocating another. Late completions are fenced by
operation/generation IDs. Never admit a third snapshot. Initial bootstrap is
separately retryable without a live application generation. Device revocation
proceeds through current store state/epoch checks and never waits for a
configuration or certificate slot.

## 10. Restart and reload boundary

The field classification is:

| Requires restart | May reload as one complete generation |
| --- | --- |
| Server hostname, JMAP origin, `online_background` view mode | Server public-address publication hints |
| Account ID/username | Account display name |
| Data/runtime/log roots | Logging threshold |
| Every listener stanza field and listener creation/removal | Domain routes and MX/MTA-STS policy, aliases and identities |
| Every resource-plan field in limits/disk/work/network | Certificate profiles/material, gateway policies and relay settings/credentials |
| ACME directory/contact/terms/trust configuration and resolver endpoints/order | — |

An account-ID change additionally requires the offline account operation
above. Do not partially apply a candidate that mixes restart-only and
reloadable changes. Return a fixed restart-required result while retaining the
old state. Reloadable reference changes still require the entire resulting
graph to be valid, including certificate names and all pool/role constraints.
Derived HTTPS SNI/certificate dispatch tables belong to the reloadable
generation. Updating those tables through domains/certificate profiles does
not change a listener stanza or restart its existing socket; changing a
listener's explicit primary certificate field still requires restart.

In-flight work pins its admitted generation for a bounded lifetime;
authorization is rechecked at mutation commit as API.md requires. QUEUE.md
owns immutable committed envelope and recipient responsibility, including
configuration repair of a paused route. A reload cannot discard a queued
submission or mark an uncertain delivery safe to repeat. Any semantic change
to the selected gateway policy's trust material, accepted pins or prefix set
invalidates that connection's admission proof, even if a change only broadens
access. Removing/changing its binding also invalidates it. Unrelated policy
changes and mere equivalent prefix spelling/order do not. Refuse an affected
in-progress DATA transaction temporarily before commit, then close with 421 at
the next legal SMTP reply boundary (never inject a reply into DATA bytes). The
peer reconnects and completes a new TLS handshake under the current policy;
STARTTLS cannot be repeated on an already-TLS connection. This revocation
fence is distinct from pinned recipient routing decisions. Keeping an old
snapshot alive does not extend revoked authority. Device revocation remains a
service-managed authorized operation.

M19 must implement and test every field's stated classification,
two-generation capacity, old/new overlap and restart refusal before exposing
the reload command. This schema does not implement those runtime transitions.

## 11. Structural fixture

This complete structural example uses reserved example names and provisioned
certificate paths. It is a loader fixture, not a deployable configuration or a
claim that those files exist. The certificate must ultimately cover both host
names, and device creation is a separate local operation.

```text
version = 1

[server]
hostname = "mx.example.test"
jmap_origin = "https://mail.example.test"

[resolver "fixture"]
address = "127.0.0.1:5353"

[account "01010101010101010101010101010101"]
username = "me@example.test"
name = "Personal mail"

[domain "example.test"]

[alias "me@example.test"]
account = "01010101010101010101010101010101"

[identity "11111111111111111111111111111111"]
account = "01010101010101010101010101010101"
name = "Me"
email = "me@example.test"

[relay]
host = "relay.example.test"
port = 465
username = "me@example.test"
password_file = "/etc/td-mta/secrets/relay-password"

[certificate "public"]
mode = "files"
chain_file = "/etc/td-mta/certificates/chain.pem"
key_file = "/etc/td-mta/secrets/server-key.pem"

[listener "mx"]
kind = "direct_smtp"
bind = "0.0.0.0:25"
server_name = "mx.example.test"
certificate = "public"
session_limit = 8
per_peer_limit = 2

[listener "jmap"]
kind = "https"
bind = "0.0.0.0:443"
certificate = "public"
```

Implementation fixtures must also cover a gateway-only profile with an
explicit upstream MX, private client trust/pins/prefixes, and both forms of
certificate provisioning. A combined profile must divide the global SMTP
session budget explicitly; copying the example's full eight-slot limit onto a
second listener is invalid. All fixture network activity stays local as
DESIGN.md requires. M11/M18 use an unprivileged user/network namespace with
private loopback and namespace-local port 80/443 permission; fixture setup
must fail rather than use host listeners or public endpoints. No test-only
high-port bypass is added to the schema. M18 adds a dedicated HTTP-01
readiness event before activating that role; existing `listener_ready`
SMTP/JMAP records must not mislabel it.

V1 uses the existing fixed 30-day successful/canceled submission retention
policy; it has no operator retention field. Unresolved failures remain
governed by QUEUE.md and are never erased by that timer.
