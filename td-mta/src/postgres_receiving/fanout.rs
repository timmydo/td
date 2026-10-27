//! One retained SMTP publication spanning all accepted local accounts.
use super::*;
use crate::{format::row::ReceiptRecipients, ids::AccountId};

pub struct Target<'a> {
    pub binding: &'a Binding,
    pub recipients: ReceiptRecipients<'a>,
}
#[derive(Clone)]
pub struct Fanout {
    operation: ReceivingOperationId,
    length: u64,
    deliveries: Vec<Delivery>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FanoutOutcome {
    pub accounts: Vec<(AccountId, Outcome)>,
}
impl Fanout {
    pub(crate) fn epoch(&self) -> Result<crate::ids::StoreEpoch, Error> {
        self.deliveries
            .first()
            .map(|delivery| delivery.binding.epoch())
            .ok_or(Error::Integrity)
    }
    pub fn operation(&self) -> ReceivingOperationId {
        self.operation
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
    let prepared = prepare_with_meter(first.binding, message, deadline, meter, clock)?;
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
            operation: prepared.operation,
            email: prepared.email,
            blob: prepared.blob,
            new_thread: prepared.new_thread,
            bytes: prepared.bytes.clone(),
            body_digest: prepared.body_digest,
            interpretation: prepared.interpretation.clone(),
            request_digest: [0; 32],
            message_id: copy(&prepared.message_id)?,
            anchors: prepared.anchors.clone(),
            provenance: input::provenance(&private, prepared.blob, prepared.new_thread)?,
        });
    }
    deliveries.sort_unstable_by_key(|delivery| delivery.binding.account());
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for part in [
        b"td-mta PostgreSQL receiving fanout v1\0".as_slice(),
        prepared.operation.as_bytes(),
        &prepared.request_digest,
        &(deliveries.len() as u64).to_be_bytes(),
    ] {
        hash.update(part).map_err(|_| Error::Crypto)?;
    }
    for delivery in &deliveries {
        for part in [
            delivery.binding.account().as_bytes().as_slice(),
            &(delivery.provenance.encoded.len() as u64).to_be_bytes(),
            &delivery.provenance.encoded,
        ] {
            charge(&clock, meter, part.len(), 0)?;
            hash.update(part).map_err(|_| Error::Crypto)?;
        }
    }
    let digest = hash.finish().map_err(|_| Error::Crypto)?;
    for delivery in &mut deliveries {
        delivery.request_digest = digest;
    }
    Ok(Fanout {
        operation: prepared.operation,
        length: prepared.length(),
        deliveries,
    })
}

async fn receipts(client: &Client, fanout: &Fanout) -> Result<Option<FanoutOutcome>, Error> {
    let mut accounts = Vec::new();
    accounts
        .try_reserve_exact(fanout.deliveries.len())
        .map_err(|_| Error::Capacity)?;
    for delivery in &fanout.deliveries {
        if let Some(outcome) = receipt(client, delivery).await? {
            accounts.push((delivery.binding.account(), outcome));
        }
    }
    if accounts.is_empty() {
        return Ok(None);
    }
    if accounts.len() != fanout.deliveries.len() {
        return Err(Error::Integrity);
    }
    Ok(Some(FanoutOutcome { accounts }))
}

pub fn inspect_fanout(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    fanout: &Fanout,
) -> Result<Option<FanoutOutcome>, Error> {
    let fanout = fanout.clone();
    let result = postgres_probe::with_fanout_client(
        request,
        trust,
        deadline,
        fanout.count(),
        move |client| {
            Box::pin(async move {
                begin(
                    client,
                    &fanout.deliveries.first().ok_or(Error::Integrity)?.binding,
                )
                .await?;
                for delivery in &fanout.deliveries {
                    client
                        .query_one(
                            "SELECT id FROM td_mta.accounts WHERE id=$1 FOR SHARE",
                            &[&delivery.binding.account().as_bytes().as_slice()],
                        )
                        .await
                        .map_err(|_| Error::Database)?;
                }
                receipts(client, &fanout).await
            })
        },
    )
    .map_err(Error::Connection)?;
    if !matches!(result, Ok(Some(_))) && Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

pub fn publish_fanout(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    fanout: &Fanout,
) -> Result<FanoutOutcome, Error> {
    if let Some(outcome) = inspect_fanout(request, trust, deadline, fanout)? {
        return Ok(outcome);
    }
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
                publish_body(client, &fanout, deadline, &marker)
                    .await
                    .map(Publication::outcome)
            })
        },
    );
    match result {
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

pub(crate) enum Publication {
    Existing(FanoutOutcome),
    Committed(FanoutOutcome),
}
impl Publication {
    pub(crate) fn outcome(self) -> FanoutOutcome {
        match self {
            Self::Existing(value) | Self::Committed(value) => value,
        }
    }
}
pub(crate) async fn publish_body(
    client: &Client,
    fanout: &Fanout,
    deadline: Instant,
    marker: &AtomicBool,
) -> Result<Publication, Error> {
    let binding = &fanout.deliveries.first().ok_or(Error::Integrity)?.binding;
    let selected = client.query_one("SELECT generation,version_id FROM td_mta.config_selection WHERE singleton=true FOR SHARE", &[]).await.map_err(|_| Error::Database)?;
    let generation = sequence(selected.try_get(0).map_err(|_| Error::Integrity)?)?;
    let version: Option<&[u8]> = selected.try_get(1).map_err(|_| Error::Integrity)?;
    let mut states = Vec::new();
    states
        .try_reserve_exact(fanout.deliveries.len())
        .map_err(|_| Error::Capacity)?;
    // All account locks precede every receipt or publication observation.
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
    if let Some(outcome) = receipts(client, fanout).await? {
        return Ok(Publication::Existing(outcome));
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
    for (delivery, state) in fanout.deliveries.iter().zip(&states) {
        accounts.push((
            delivery.binding.account(),
            write_delivery(client, delivery, state, deadline).await?,
        ));
    }
    marker.store(true, Ordering::Relaxed);
    client
        .batch_execute("COMMIT")
        .await
        .map_err(|_| Error::Indeterminate)?;
    Ok(Publication::Committed(FanoutOutcome { accounts }))
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
