# Startup resource ledger

M01 implements `Limits::plan` in [src/limits.rs](src/limits.rs). It validates
counts, individual ceilings, relationships and checked arithmetic before
returning a fixed-size ledger. It allocates no pools and starts no workers.
`ConfigVersion` versions operator configuration, not the unimplemented disk
format. Local IDs are distinct 16-byte types with canonical lowercase hex;
they convey no authorization. Derived MIME-part wire IDs belong to M02.

## Default reservation

All quantities below are bytes. This is a planned ceiling for each component,
not a claim that the service exists or its RSS has been measured. M02 assigns
concrete structures/workers within it; M04 implements pools; M07 measures TLS;
M23 verifies resident usage. A structure exceeding its reservation must change
the checked ledger and pass the budget gate before admission is enabled.

| Component | Count | Bytes each | Total |
| --- | ---: | ---: | ---: |
| SMTP slots | 8 | 376064 | 3008512 |
| HTTPS slots | 8 | 1703936 | 13631488 |
| Body/search jobs | 2 | 425984 | 851968 |
| Storage read views | 2 | 4587520 | 9175040 |
| Writer and checkpoint | 1 | 5767168 | 5767168 |
| Resident index cache | 1 | 8388608 | 8388608 |
| Outbound slots | 1 | 163072 | 163072 |
| Sort runs and merge buffers | 1 | 1048576 | 1048576 |
| Log queue and formatting | 1 | 131072 | 131072 |
| DNS, ACME and control scratch | 1 | 524288 | 524288 |
| Slot queues and queue window | 1 | 131072 | 131072 |
| Fixed worker stacks | 8 | 262144 | 2097152 |
| Main stack allowance | 1 | 1048576 | 1048576 |
| TLS session headroom | 17 | 131072 | 2228224 |
| TLS handshake headroom | 2 | 1048576 | 2097152 |
| Certificate generations | 2 | 1048576 | 2097152 |
| Cold reload overlap | 1 | 2097152 | 2097152 |
| Process and allocator allowance | 1 | 8388608 | 8388608 |
| **Total** | | | **62874880** |

The total is approximately 59.96 MiB against a 64 MiB configured budget;
the remaining 4233984 bytes are unassigned headroom, not another cache.
The 128 MiB workload RSS release ceiling remains independent. Raising the
configured memory budget does not preserve the default RSS claim.

## Slot composition and ownership

- SMTP: bounded headers, 320 bytes per envelope recipient, 64 KiB I/O and
  16 KiB state. The fixed recipient cell must hold the SMTP path plus metadata.
- HTTPS: request bytes, 16 bytes per JSON token, 128 bytes per largest
  get/set/query result window entry, and 96 KiB framing/output scratch.
  Method/result references and escaped strings must fit these arenas; they
  cannot introduce per-method heap trees. Event streams consume these slots
  without pinning storage views between emissions.
- Body job: headers, 64 bytes per MIME descriptor, 96 KiB decode/work scratch.
  Nested parsing and transfer decoding share that reservation.
- Read view: 4 MiB journal prefix, 32 bytes per journal operation, 128 KiB
  cursor/value scratch. Backup consumes an existing view.
- Writer: one journal arena and descriptor array, one 1 MiB frame, and
  256 KiB table/manifest/value scratch. Pending commits reference fixed slot
  buffers; there is no extra frame allocation for every waiting connection.
- Outbound: envelope recipient cells and 128 KiB transfer/reply scratch.
- Sort scratch: one shared MiB for run formation and bounded merge buffers.
  Disk spill space is independently capped at 64 MiB by default.
- Fixed control scratch and queues cover bounded DNS cache/replies, ACME
  HTTP/JWS/CSR work, administrative formatting, retry-window IDs and slot
  descriptors. M02 must split those reservations and set their count caps.
- Eight worker stacks are a reservation, not a scheduling implementation.
  M02 chooses worker roles; no worker/thread may appear outside this count
  without a ledger amendment. The main stack allowance is a resident budget,
  not a claim that the host's virtual stack mapping is one MiB.
- TLS sessions include SMTP, HTTPS and outgoing delivery slots; handshake
  scratch is additional. The handshake cap is global in this profile.
  HTTP-01/administration must use the existing fixed control/I/O reservations;
  they cannot silently add another general connection pool.
- Certificate overlap, reload overlap, allocator bookkeeping, executable
  pages and main/worker stacks all count at peak coexistence. RSS tests must
  validate the allowances; this ledger is not an OS memory limiter.

Message size, upload/queue quotas, queue length and log file limits are disk or
admission bounds. Growing them does not reserve whole bodies or a whole queue in
RAM. The default log disk reservation is five 8 MiB files (active plus four
retained). Free-space/metadata/inode quotas and storage maintenance reservations
are completed in M02/M05/M08 before any mail can be accepted.

The per-upload byte ceiling is `message_bytes`, initially 32 MiB; M13 publishes
that value as `maxSizeUpload` and enforces it even for attachment uploads.
`upload_disk_bytes` is the separate aggregate quota for retained upload blobs.

The compiled maxima and checked default values live in `limits.rs`; this table
records the default byte ledger only. Journal/frame limits are fixed to the
storage contract. SMTP retains room for at least 100 recipients. Disabled event
streams may use zero slots; mandatory pools and byte budgets cannot be zero.
The bounds are startup validation, not protocol error mappings or proof that
all combinations meet standards. M02 adds operation-specific work/field limits
and M13 publishes only limits that its admission code actually enforces.

## Evidence

The unit cases exercise malformed/canonical IDs, unsupported configuration
versions, invalid pool relationships, arithmetic overflow, insufficient
memory, expansion beyond the default budget, and streaming quotas independent
of resident reservations. No claim about runtime allocation count, TLS or RSS
is made by these tests. Both host and sandbox cargo rosters discover the
standalone crate from its manifest; no manual crate list is needed.
