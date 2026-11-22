//! One atomic SMTP publication spanning all accepted local accounts.
use super::*;
use crate::{format::row::ReceiptRecipients, ids::AccountId, smtp_storage::Recipient};

pub struct Target<'a> {
    pub binding: &'a Binding,
    pub recipients: ReceiptRecipients<'a>,
}
#[derive(Clone)]
pub struct Fanout {
    length: u64,
    deliveries: Vec<Delivery>,
}
/// A local notice stored instead of the message in an account whose quota
/// refused it after RCPT.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Notice {
    pub email: EmailId,
    pub old_sequence: u64,
    pub new_sequence: u64,
}
/// Every target appears once: delivered in `accounts` or noticed in `notices`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanoutOutcome {
    pub accounts: Vec<(AccountId, Outcome)>,
    pub notices: Vec<(AccountId, Notice)>,
}
impl Fanout {
    pub(crate) fn epoch(&self) -> Result<crate::ids::StoreEpoch, Error> {
        self.deliveries
            .first()
            .map(|delivery| delivery.binding.epoch())
            .ok_or(Error::Integrity)
    }
    pub fn count(&self) -> usize {
        self.deliveries.len()
    }
    pub fn length(&self) -> u64 {
        self.length
    }
}

/// Targets carry the trusted routing owner's per-account accepted recipients.
/// The DATA body is parsed and retained once; private receipt metadata is copied.
pub fn prepare_fanout(
    targets: &[Target<'_>],
    message: &Message<'_>,
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
) -> Result<Fanout, Error> {
    prepare_observed(targets, message, deadline, meter, clock, None)
}

pub(crate) fn prepare_observed(
    targets: &[Target<'_>],
    message: &Message<'_>,
    deadline: Instant,
    meter: &mut Meter,
    clock: &RuntimeClock,
    results: Option<&crate::sender_authentication::Results>,
) -> Result<Fanout, Error> {
    if targets.is_empty() || targets.len() > crate::config::identities::MAX_ACCOUNTS {
        return Err(Error::Capacity);
    }
    let first = targets.first().ok_or(Error::Integrity)?;
    let mut total = 0usize;
    for (index, target) in targets.iter().enumerate() {
        let binding = target.binding;
        if binding.epoch() != first.binding.epoch()
            || binding.selection() != first.binding.selection()
            || binding.profile() != first.binding.profile()
            || !binding.roles().receive()
            || target.recipients.count() == 0
            || targets
                .iter()
                .take(index)
                .any(|prior| prior.binding.account() == binding.account())
        {
            return Err(Error::Policy);
        }
        total = total
            .checked_add(target.recipients.count() as usize)
            .ok_or(Error::Capacity)?;
    }
    if total != message.receipt.recipients.count() as usize
        || total > first.binding.recipient_maximum()
    {
        return Err(Error::Policy);
    }
    // Check the partition rather than trusting equal recipient counts. Repeated
    // RCPT spellings remain legal, so compare sorted multisets including repeats.
    let mut original = Vec::new();
    original
        .try_reserve_exact(total)
        .map_err(|_| Error::Capacity)?;
    for address in message
        .receipt
        .recipients
        .iter()
        .map_err(|_| Error::Integrity)?
    {
        original.push(address.map_err(|_| Error::Integrity)?);
    }
    let mut partition = Vec::new();
    partition
        .try_reserve_exact(total)
        .map_err(|_| Error::Capacity)?;
    for target in targets {
        for address in target.recipients.iter().map_err(|_| Error::Integrity)? {
            partition.push(address.map_err(|_| Error::Integrity)?);
        }
    }
    original.sort_unstable();
    partition.sort_unstable();
    if original != partition {
        return Err(Error::Policy);
    }
    let prepared =
        super::prepare_observed(first.binding, message, deadline, meter, clock, results)?;
    let mut deliveries = Vec::new();
    deliveries
        .try_reserve_exact(targets.len())
        .map_err(|_| Error::Capacity)?;
    let clock = PreparationClock { clock, deadline };
    for target in targets {
        charge(
            &clock,
            meter,
            4 * crate::format::MAX_VALUE_BYTES,
            2 * crate::format::MAX_VALUE_BYTES,
        )?;
        let private = Message {
            bytes: message.bytes,
            receipt: crate::format::row::SmtpReceipt {
                recipients: target.recipients,
                ..message.receipt
            },
            hostname: message.hostname,
            extended: message.extended,
            header_bytes: message.header_bytes,
            received_at: message.received_at,
        };
        deliveries.push(Delivery {
            binding: target.binding.clone(),
            email: prepared.email,
            blob: prepared.blob,
            new_thread: prepared.new_thread,
            bytes: prepared.bytes.clone(),
            body_digest: prepared.body_digest,
            interpretation: prepared.interpretation.clone(),
            message_id: copy(&prepared.message_id)?,
            anchors: prepared.anchors.clone(),
            provenance: input::provenance(&private, prepared.blob, prepared.new_thread)?,
            authentication: results
                .map(crate::sender_authentication::Results::try_copy)
                .transpose()
                .map_err(Error::Input)?,
        });
    }
    deliveries.sort_unstable_by_key(|delivery| delivery.binding.account());
    Ok(Fanout {
        length: prepared.length(),
        deliveries,
    })
}

pub fn publish_fanout(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    fanout: &Fanout,
) -> Result<FanoutOutcome, Error> {
    let fanout = fanout.clone();
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_fanout_client(
        request,
        trust,
        deadline,
        fanout.count(),
        move |client| {
            Box::pin(async move {
                let binding = &fanout.deliveries.first().ok_or(Error::Integrity)?.binding;
                begin(client, binding).await?;
                publish_body(client, &fanout, deadline, &marker).await
            })
        },
    );
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

pub(crate) async fn publish_body(
    client: &Client,
    fanout: &Fanout,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<FanoutOutcome, Error> {
    let binding = &fanout.deliveries.first().ok_or(Error::Integrity)?.binding;
    let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    let mut states = Vec::new();
    states
        .try_reserve_exact(fanout.deliveries.len())
        .map_err(|_| Error::Capacity)?;
    // Acquire all account locks before writing any delivery.
    // Preparation sorts the targets, independent of envelope RCPT order.
    for delivery in &fanout.deliveries {
        states.push(
            client
                .query_one(
                    "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR UPDATE",
                    &[&delivery.binding.account().as_bytes().as_slice()],
                )
                .await
                .map_err(|_| Error::Database)?,
        );
    }
    if generation != binding.selection().generation
        || version != Some(binding.selection().version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    let mut accounts = Vec::new();
    accounts
        .try_reserve_exact(fanout.deliveries.len())
        .map_err(|_| Error::Capacity)?;
    let mut refused =
        td_alloc::with_capacity(fanout.deliveries.len()).map_err(|_| Error::Capacity)?;
    for (delivery, state) in fanout.deliveries.iter().zip(&states) {
        // The quota charge precedes every write, so a refused account is untouched.
        match write_delivery(client, delivery, state, deadline).await {
            Ok(outcome) => accounts.push((delivery.binding.account(), outcome)),
            Err(Error::Quota) => {
                td_alloc::push(&mut refused, (delivery, state)).map_err(|_| Error::Capacity)?
            }
            Err(error) => return Err(error),
        }
    }
    // With no account to receive it the whole message stays refused.
    if accounts.is_empty() {
        return Err(Error::Quota);
    }
    let mut notices = td_alloc::with_capacity(refused.len()).map_err(|_| Error::Capacity)?;
    for (delivery, state) in refused {
        let facts = &delivery.provenance;
        let old = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
        if sequence(state.try_get(1).map_err(|_| Error::Integrity)?)? > old {
            return Err(Error::Integrity);
        }
        let (email, new_sequence) = crate::postgres_failure_notice::store_refused(
            client,
            crate::postgres_failure_notice::Refused {
                binding: &delivery.binding,
                reverse_path: &facts.reverse,
                recipients: &facts.recipients,
                length: delivery.length(),
                old,
            },
            deadline,
        )
        .await
        .map_err(notice_error)?;
        td_alloc::push(
            &mut notices,
            (
                delivery.binding.account(),
                Notice {
                    email,
                    old_sequence: old,
                    new_sequence,
                },
            ),
        )
        .map_err(|_| Error::Capacity)?;
    }
    let mut timing =
        crate::performance::Timer::start(crate::performance::Phase::SmtpDatabaseCommit);
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)?;
    timing.succeeded();
    Ok(FanoutOutcome { accounts, notices })
}

fn notice_error(error: crate::postgres_failure_notice::Error) -> Error {
    use crate::postgres_failure_notice::Error as N;
    match error {
        N::Quota => Error::Quota,
        N::Capacity => Error::Capacity,
        N::Work(ports::Error::Deadline) => Error::Connection(postgres_probe::Error::Deadline),
        N::Work(error) => Error::Input(error),
        N::Crypto => Error::Crypto,
        N::Database => Error::Database,
        N::Connection(error) => Error::Connection(error),
        N::Policy => Error::Policy,
        N::Owner(_) | N::Integrity | N::Indeterminate => Error::Integrity,
    }
}

/// The binding facts an RCPT check reads, copied into its database job.
#[derive(Clone, Copy)]
pub(crate) struct RecipientScope {
    pub(crate) account: AccountId,
    pub(crate) epoch: crate::ids::StoreEpoch,
    selection: crate::postgres_selection::Selection,
    quotas: crate::postgres_configuration::SharedQuotas,
}
impl RecipientScope {
    pub(crate) fn new(binding: &Binding) -> Result<Self, Error> {
        if !binding.roles().receive() {
            return Err(Error::Policy);
        }
        Ok(Self {
            account: binding.account(),
            epoch: binding.epoch(),
            selection: binding.selection(),
            quotas: binding.quotas(),
        })
    }
}

/// Advisory check of one offered RCPT account: SELECTs only, without
/// account locks so it never waits for another delivery's commit, then the
/// session rolls back. Its statements may each see a later commit;
/// publication rechecks the selection, Inbox and quota under its locks.
pub(crate) async fn recipient_body(
    client: &Client,
    scope: RecipientScope,
    declared_bytes: Option<u64>,
) -> Result<Recipient, Error> {
    let selected = client
        .query_one(
            "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    if generation != scope.selection.generation
        || version != Some(scope.selection.version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    let account = scope.account;
    // Count roles in one statement rather than compare the Inbox's history
    // with an account row read by another, which a commit could separate.
    let inboxes: i64 = client
        .query_one(
            "SELECT count(*) FROM td_mta.mailboxes WHERE account=$1 AND role='inbox'",
            &[&account.as_bytes().as_slice()],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Integrity)?;
    if inboxes != 1 {
        return Ok(Recipient::Unavailable);
    }
    let caps = scope.quotas;
    // A message without SIZE still needs room for one byte and one blob.
    let bytes = declared_bytes.unwrap_or(1).max(1);
    match publication::charge(client, account, caps, [bytes, 1, 0, 0, 0]).await {
        Ok(_) => Ok(Recipient::Accepted),
        Err(publication::Error::Quota)
            if declared_bytes.is_some_and(|declared| declared > caps.body_bytes) =>
        {
            Ok(Recipient::ExceedsQuota)
        }
        Err(publication::Error::Quota) => Ok(Recipient::Full),
        Err(error) => Err(publication_error(error)),
    }
}

/// Check all configured Inboxes under one connection and original deadline.
/// This is startup/admission evidence; publication rechecks the entire transaction.
pub fn ready_many(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    bindings: &[&Binding],
) -> Result<(), Error> {
    let values = ready_bindings(bindings)?;
    let result =
        postgres_probe::with_fanout_client(request, trust, deadline, values.len(), move |client| {
            Box::pin(async move {
                begin(client, values.first().ok_or(Error::Integrity)?).await?;
                ready_body(client, &values).await
            })
        })
        .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

pub(crate) fn ready_bindings(bindings: &[&Binding]) -> Result<Vec<Binding>, Error> {
    if bindings.is_empty() || bindings.len() > crate::config::identities::MAX_ACCOUNTS {
        return Err(Error::Capacity);
    }
    let first = bindings.first().ok_or(Error::Integrity)?;
    let mut values = Vec::new();
    values
        .try_reserve_exact(bindings.len())
        .map_err(|_| Error::Capacity)?;
    for (index, binding) in bindings.iter().enumerate() {
        if !binding.roles().receive()
            || binding.epoch() != first.epoch()
            || binding.selection() != first.selection()
            || binding.profile() != first.profile()
            || bindings
                .iter()
                .take(index)
                .any(|prior| prior.account() == binding.account())
        {
            return Err(Error::Policy);
        }
        values.push((*binding).clone());
    }
    values.sort_unstable_by_key(Binding::account);
    Ok(values)
}
pub(crate) async fn ready_body(client: &Client, values: &[Binding]) -> Result<(), Error> {
    let first = values.first().ok_or(Error::Integrity)?;
    let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    if generation != first.selection().generation
        || version != Some(first.selection().version.as_bytes().as_slice())
    {
        return Err(Error::Policy);
    }
    for binding in values {
        let state = client
            .query_one(
                "SELECT sequence,floor FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                &[&binding.account().as_bytes().as_slice()],
            )
            .await
            .map_err(|_| Error::Database)?;
        let sequence = sequence(state.try_get(0).map_err(|_| Error::Integrity)?)?;
        if super::sequence(state.try_get(1).map_err(|_| Error::Integrity)?)? > sequence {
            return Err(Error::Integrity);
        }
        inbox(client, binding.account(), sequence).await?;
    }
    Ok(())
}
