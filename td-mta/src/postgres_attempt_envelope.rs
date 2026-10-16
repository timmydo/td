//! Initial SMTP envelope hints for an exact active Prepared attempt.
use super::{Dispatch, Error};
use crate::{
    config::values::mailbox_key,
    format::row::MAX_ADDRESS,
    ids::{AttemptId, SubmissionId},
    postgres_probe::{self, Request},
    postgres_route_owners as owners,
};
use std::time::Instant;
use td_crypto::TrustStore;

#[derive(Debug, Eq, PartialEq)]
pub struct Recipient {
    pub ordinal: u32,
    pub address: String,
    pub uncertain: bool,
}
#[derive(Debug, Eq, PartialEq)]
pub struct Envelope {
    pub attempt: AttemptId,
    pub submission: SubmissionId,
    pub reverse_path: String,
    pub recipients: Vec<Recipient>,
}
fn address(value: &str, empty: bool) -> Result<String, Error> {
    if !(empty && value.is_empty()) {
        mailbox_key(value, &mut [0; MAX_ADDRESS]).map_err(|_| Error::Integrity)?;
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| Error::Capacity)?;
    owned.push_str(value);
    Ok(owned)
}
/// Reads only an untouched Prepared batch. A partial RCPT result or later
/// phase refuses reconstruction; retain the admitted envelope for that attempt.
/// Returned values grant no SMTP permission and all locks end with this call.
pub fn envelope(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    value: &Dispatch,
) -> Result<Envelope, Error> {
    let value = value.clone();
    postgres_probe::with_mailbox_client(request,trust,deadline,move |client|Box::pin(async move {
        if !owners::begin(client,&value.claim,true,false).await.map_err(Error::Owner)? {return Err(Error::Policy);}
        let timestamp=owners::check_locked(client,&value.claim,value.generation).await.map_err(Error::Owner)?;
        let outcome=super::active_receipt(client,&value).await?;
        let binding=value.claim.binding();
        let account=binding.account();
        let account=account.as_bytes().as_slice();
        let submission=value.submission.as_bytes().as_slice();
        let rows=client.query("SELECT s.reverse_path,s.recipient_count,s.send_at,s.expires_at,s.completed_at,s.changed,a.sequence,a.floor FROM td_mta.submissions s JOIN td_mta.accounts a ON a.id=s.account JOIN td_mta.submission_routes r ON r.account=s.account AND r.submission_id=s.id WHERE s.account=$1 AND s.id=$2 AND r.route_name=$3 AND r.destination=$4", &[&account,&submission,&binding.route().name(),&binding.route().destination().as_slice()]).await.map_err(|_|Error::Database)?;
        let row=rows.first().ok_or(Error::Missing)?;
        let reverse:&str=row.try_get(0).map_err(|_|Error::Integrity)?;
        let total:i64=row.try_get(1).map_err(|_|Error::Integrity)?;
        let send:i64=row.try_get(2).map_err(|_|Error::Integrity)?;
        let expires:i64=row.try_get(3).map_err(|_|Error::Integrity)?;
        let completed:Option<i64>=row.try_get(4).map_err(|_|Error::Integrity)?;
        let sequence=super::number(row.try_get(6).map_err(|_|Error::Integrity)?)?;
        if !(1..=1000).contains(&total) || send<0 || expires<send || outcome.prepared_at<0 || outcome.prepared_at>timestamp
            || outcome.new_sequence>sequence
            || super::number(row.try_get(5).map_err(|_|Error::Integrity)?)?>sequence
            || super::number(row.try_get(7).map_err(|_|Error::Integrity)?)?>sequence {return Err(Error::Integrity);}
        if completed.is_some() || timestamp<send || timestamp>=expires {return Err(Error::NotDue);}
        let reverse_path=address(reverse,true)?;
        let mut selected=Vec::new();
        selected.try_reserve_exact(outcome.ordinals.len()).map_err(|_|Error::Capacity)?;
        for ordinal in &outcome.ordinals {selected.push(i64::from(*ordinal));}
        let rows=client.query("SELECT ordinal,address,state,attempt_id,phase,attempt_count,last_attempt_at,next_attempt_at,changed,reason,diagnostic,uncertain FROM td_mta.recipients WHERE account=$1 AND submission_id=$2 AND ordinal=ANY($3) ORDER BY ordinal LIMIT 101", &[&account,&submission,&selected]).await.map_err(|_|Error::Database)?;
        if rows.len()!=outcome.ordinals.len(){return Err(Error::Integrity);}
        let mut recipients=Vec::new();
        recipients.try_reserve_exact(rows.len()).map_err(|_|Error::Capacity)?;
        for (row,expected) in rows.iter().zip(&outcome.ordinals) {
            let ordinal:i64=row.try_get(0).map_err(|_|Error::Integrity)?;
            let state:i64=row.try_get(2).map_err(|_|Error::Integrity)?;
            let attempt:Option<&[u8]>=row.try_get(3).map_err(|_|Error::Integrity)?;
            let phase:i64=row.try_get(4).map_err(|_|Error::Integrity)?;
            let count:i64=row.try_get(5).map_err(|_|Error::Integrity)?;
            let last:Option<i64>=row.try_get(6).map_err(|_|Error::Integrity)?;
            let next:Option<i64>=row.try_get(7).map_err(|_|Error::Integrity)?;
            let reason:i64=row.try_get(9).map_err(|_|Error::Integrity)?;
            let diagnostic:&str=row.try_get(10).map_err(|_|Error::Integrity)?;
            let uncertain:i64=row.try_get(11).map_err(|_|Error::Integrity)?;
            if state!=2 || phase!=1 || attempt!=Some(value.attempt.as_bytes().as_slice()) {return Err(Error::Conflict);}
            if ordinal!=i64::from(*expected) || ordinal>=total || !(1..=i64::from(u32::MAX)).contains(&count)
                || last!=Some(outcome.prepared_at) || next.is_some() || reason!=0 || !diagnostic.is_empty()
                || !(0..=1).contains(&uncertain)
                || super::number(row.try_get(8).map_err(|_|Error::Integrity)?)?!=outcome.new_sequence {return Err(Error::Integrity);}
            recipients.push(Recipient{ordinal:*expected,address:address(row.try_get(1).map_err(|_|Error::Integrity)?,false)?,uncertain:uncertain==1});
        }
        let finished=owners::check_locked(client,&value.claim,value.generation).await.map_err(Error::Owner)?;
        if finished>=expires {return Err(Error::NotDue);}
        Ok(Envelope{attempt:value.attempt,submission:value.submission,reverse_path,recipients})
    })).map_err(Error::Connection)?
}
