# Stalwart replacement runbook

This is an operator checklist, not authorization to change production domains.
Do not switch `timmydouglas.com` or `timmyd.info` until the owner explicitly
approves the concrete account inventory, routing changes and maintenance window.
`mx.882015.xyz` is the separate td-mta deployment used for acceptance.

The current migration commands are documented in [MIGRATION.md](MIGRATION.md).
A successful provisional command never makes the whole deployment ready.

## Current evidence and remaining blockers

The initial independent Stalwart 0.15.2 copy preserved 816 messages across
eight folders, including duplicate messages, shared memberships, folder moves,
keywords and more than one GiB of logical raw mail. Separate account-wide
inventory and cross-server archive comparison checks passed. Source message
deletion reconciliation preserves target-only mail and retains import receipts
across retries.

A target-only new message survived rollback with identical raw bytes, flags
and Inbox membership, but Stalwart 0.15.2 returned its received time at whole
second precision: `2026-10-22T07:47:21.998Z` became
`2026-10-22T07:47:21Z`. This is a failed exact-date acceptance check. Preserve
the exact td-mta export; do not round comparison timestamps to conceal the
loss. Exact rollback remains a blocker unless the owner explicitly accepts
this limited loss for emergency rollback.

The source-deleted empty-folder rehearsal removed a child before its parent
through separate retained plans, preserving all 816 surviving target messages
and their memberships, bodies and quota rows. The live production account/alias
inventory, active forwarding rules and external provider authorization must
still be reviewed before scheduling a switch.
The independent Stalwart binary and scripts remain outside build/test code.

## Prepare without changing mail routing

1. Record every production account, recipient alias, sending identity and
   forwarding rule. Identify IMAP/POP/Sieve or other Stalwart features in use;
   JMAP export does not copy settings, credentials, rules, contacts or calendars.
2. Create the corresponding td-mta accounts, aliases and sending identities
   through the supported owner commands. Retain their operation files. Set
   quotas and device credentials; verify that each device sees only its account.
3. Confirm the outbound provider permits every proposed sender and recipient.
   If SES is still in its sandbox, it cannot provide unrestricted delivery. Record
   the chosen provider and region, and validate a real delivery and bounce.
4. Confirm inbound TCP 25, HTTPS and certificate renewal on the target. Test
   unknown-recipient rejection and unauthenticated external-recipient rejection.
   Review every forward separately: accepted local aliases must never grant
   arbitrary external relay authority. Do not rely on DNS or TLS as relay policy.
5. Retain recoverable source state, PostgreSQL backup, configuration and private
   migration archives. Test restoration before relying on them. Scheduling
   automatic backups and alerts is a separate, owner-deferred configuration step.
6. Export each source account to its own private archive and independently
   check its account inventory. Import into its explicit target account. Resolve
   all unrepresentable objects and local conflicts; do not drop exceptions.

## Quiescent final pass

Agree on a maintenance window and prevent source writes, including clients,
rules, forwarding and inbound delivery. Queue or defer new SMTP delivery with
explicit temporary failures; never accept a message then discard it. Confirm
all source writers are fenced rather than inferring quiet from a single state.

Keep the preceding archive and its source binding, then refresh a private copy
under that same binding. Import new objects, reconcile retained message and
folder metadata plans, and reconcile explicitly reviewed source deletions.
Keep every plan and result. An uncertain operation is retried with its exact
retained identity, never replaced with a fresh request merely to hide uncertainty.

Run inventory on the quiet source. Export the quiet target, run inventory there,
and compare both archives. Run the target account audit with explicit row,
parent, blob, byte and time bounds. Compare counts, complete folder metadata,
raw digests, received dates, keywords and memberships; investigate every mismatch.
Separately check accounts, aliases, identities, rules and provider routes.

Only after these checks and explicit owner approval should routing and clients
move. Record previous DNS values, TTLs, client endpoints and service selections.
DNS caches can deliver to the old host during transition: maintain a deliberate
queue/defer/drain policy there. Do not leave two independent writable mail stores
and assume a later export will safely merge them.

## Acceptance after switching

Verify external incoming mail, authorized outgoing mail, a bounce, replies,
attachments, folder/flag changes, search and account isolation through td-mail.
Repeat open-relay and unknown-recipient rejection checks from outside the host.
Inspect queue state and application logs. Retain the old source and rollback
materials until the owner accepts the migration and its retention period.

## Rollback after new mail reaches td-mta

Fence td-mta writers and delivery before taking the rollback export. Preserve
all target-only messages and local changes; restoring only the old Stalwart
snapshot would lose mail accepted after the switch. Export and inventory the
quiet td-mta account, retain exact raw bodies and metadata, and reconcile into
the rollback server using retained object mappings to avoid duplicate imports.

Independently export and compare the rollback destination. Apply the received-
time precision acceptance decision above explicitly; a failed exact comparison
must remain a reported failure, even if emergency operation is approved. Do not
send queued submissions twice while moving responsibility between services.
Restore routing and client endpoints only after the destination and queue
ownership are verified. Keep both stores and the exact rollback archive for
investigation; rollback does not authorize deleting evidence.
