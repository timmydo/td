//! Complete source mailbox graph prepared outside the local transaction.
use crate::{
    config::endpoint::Origin,
    ids::{AccountId, MailboxId},
    postgres_replica_source::Source,
    replica_source::MailboxPage,
    sync::{DataState, DataType},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Source,
    MissingInbox,
}
pub struct Graph {
    pub(crate) source: Source,
    pub(crate) account: AccountId,
    pub(crate) generation: u64,
    pub(crate) observed: DataState,
    pub(crate) inbox: MailboxId,
    pub(crate) page: MailboxPage,
}
impl Graph {
    pub fn prepare(
        source: &Source,
        account: AccountId,
        generation: u64,
        page: MailboxPage,
    ) -> Result<Self, Error> {
        if generation == 0 {
            return Err(Error::Invalid);
        }
        let origin = Origin::parse(source.origin()).map_err(|_| Error::Source)?;
        if !origin.same_origin(Origin::parse(page.source_origin()).map_err(|_| Error::Source)?) {
            return Err(Error::Source);
        }
        let view = page.view().map_err(|_| Error::Invalid)?;
        let observed = view.observed;
        if observed.account != account
            || observed.epoch != source.epoch()
            || observed.kind != DataType::Mailbox
        {
            return Err(Error::Source);
        }
        let inbox = view
            .list
            .iter()
            .find(|mailbox| mailbox.row.role == Some("inbox"))
            .map(|mailbox| mailbox.id)
            .ok_or(Error::MissingInbox)?;
        Ok(Self {
            source: Source::new(source.origin(), source.epoch()).map_err(|_| Error::Source)?,
            account,
            generation,
            observed,
            inbox,
            page,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{format::Sequence, ids::StoreEpoch, replica_source::Binding, wire};
    use td_json::json;
    fn page(origin: &str, inbox: bool) -> MailboxPage {
        let account = AccountId::from_bytes([1; 16]);
        let epoch = StoreEpoch::from_bytes([2; 16]);
        let state = DataState {
            account,
            epoch,
            kind: DataType::Mailbox,
            sequence: Sequence::from_u64(3),
        };
        let mut state_bytes = [0; DataState::WIRE_BYTES];
        let state = state.encode(&mut state_bytes).unwrap();
        let mut id_bytes = [0; wire::IdKind::WIRE_BYTES];
        let account_text = wire::encode_id(account, &mut id_bytes).unwrap().to_owned();
        let id = wire::encode_id(MailboxId::from_bytes([3; 16]), &mut id_bytes).unwrap();
        let role = if inbox { Some("inbox") } else { None };
        let response = json!({"methodResponses":[["Mailbox/get",{"accountId":account_text,"state":state,"list":[{"id":id,"name":"Inbox","parentId":null,"role":role,"sortOrder":0,"isSubscribed":true}],"notFound":[]},"replica"]]});
        MailboxPage::decode(origin, Binding { account, epoch }, response).unwrap()
    }
    #[test]
    fn graph_binds_origin_account_generation_and_required_inbox() {
        let origin = "https://source.example.test";
        let source = Source::new(origin, StoreEpoch::from_bytes([2; 16])).unwrap();
        let account = AccountId::from_bytes([1; 16]);
        assert!(Graph::prepare(&source, account, 1, page(origin, true)).is_ok());
        assert_eq!(
            Graph::prepare(&source, account, 0, page(origin, true)).err(),
            Some(Error::Invalid)
        );
        assert_eq!(
            Graph::prepare(
                &source,
                AccountId::from_bytes([4; 16]),
                1,
                page(origin, true)
            )
            .err(),
            Some(Error::Source)
        );
        assert_eq!(
            Graph::prepare(
                &source,
                account,
                1,
                page("https://foreign.example.test", true)
            )
            .err(),
            Some(Error::Source)
        );
        assert_eq!(
            Graph::prepare(&source, account, 1, page(origin, false)).err(),
            Some(Error::MissingInbox)
        );
    }
}
