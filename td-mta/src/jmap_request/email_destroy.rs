//! Pure Email/set destroy requests used by td-mail, with prepaid response space.
use super::*;
use crate::{
    format::Sequence,
    ids::StoreEpoch,
    store_fs::{EmailDestroyResult, MAX_EMAIL_DESTROYS},
};
struct Destroy {
    name: Name,
    local: Option<EmailId>,
}
impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn email_destroy(
        &mut self,
        range: Range<u64>,
        account: Name,
        expected: Option<(StoreEpoch, Sequence)>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let (mutator, device) = self.flags.ok_or(Error::UnknownMethod)?;
        let maximum = maximum.min(MAX_EMAIL_DESTROYS);
        let mut inputs: Vec<Destroy> = Vec::new();
        let mut local = Vec::new();
        inputs
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        local
            .try_reserve_exact(maximum)
            .map_err(|_| Error::ResponseCapacity)?;
        let mut objects = Children::new(range, scan::Kind::Array)?;
        let mut count = 0;
        while let Some(item) = objects.next(self)? {
            count += 1;
            if count > maximum {
                return Err(Error::RequestTooLarge);
            }
            let name = self.push_string(&item)?;
            let id: Option<EmailId> = name.wire_id()?.local();
            let mut duplicate = false;
            for input in &inputs {
                self.check()?;
                if input.name.bytes() == name.bytes() {
                    duplicate = true;
                    break;
                }
            }
            if duplicate {
                continue;
            }
            if let Some(id) = id {
                local.push(id);
            }
            inputs.push(Destroy { name, local: id });
        }
        let mut bound = 256 + account.bytes().len() + 2 * DataState::WIRE_BYTES;
        for input in &inputs {
            bound = bound
                .checked_add(input.name.bytes().len() + 96)
                .ok_or(Error::ResponseCapacity)?;
        }
        if bound as u64 > self.available()? {
            return Err(Error::ResponseCapacity);
        }
        let mut output = Vec::new();
        output
            .try_reserve_exact(bound)
            .map_err(|_| Error::ResponseCapacity)?;
        output.resize(bound, 0);
        let reserved = self
            .retention
            .reserve_mutation_output(bound)
            .map_err(Error::Storage)?;
        let request = EmailDestroyRequest {
            account: self.account,
            device,
            expected,
            emails: &local,
            deadline: self.execution,
        };
        let complete = self
            .retention
            .with_mutation_meter(self.execution, |meter| mutator.destroy(request, meter))
            .map_err(|error| failure(error, expected.is_some()))?;
        let sequence = match complete.outcome {
            Ok(sequence) => sequence,
            Err(crate::store_fs::CommitError::Rejected(error)) => {
                return Err(failure(error.into(), expected.is_some()))
            }
            Err(crate::store_fs::CommitError::Indeterminate(_)) => {
                return Err(Error::ServerPartialFail)
            }
        };
        let encoded = (|| {
            if complete.results.len() != local.len() {
                return Err(Error::State);
            }
            let old = DataState {
                account: self.account,
                epoch: complete.before.epoch,
                kind: DataType::Email,
                sequence: complete.before.committed_sequence,
            };
            let new = DataState { sequence, ..old };
            let mut before = [0; DataState::WIRE_BYTES];
            let mut after = [0; DataState::WIRE_BYTES];
            let old = old.encode(&mut before).map_err(|_| Error::State)?;
            let new = new.encode(&mut after).map_err(|_| Error::State)?;
            // Admission already checked these ID bytes against the ASCII grammar.
            let account = std::str::from_utf8(account.bytes()).map_err(|_| Error::State)?;
            let mut out = TextBuffer::new(&mut output);
            out.format(format_args!(
                "{{\"accountId\":\"{account}\",\"oldState\":\"{old}\",\"newState\":\"{new}\",\"created\":null,\"updated\":null,\"notCreated\":null,\"notUpdated\":null"
            )).map_err(|_| Error::State)?;
            for errors in [false, true] {
                out.append(if errors {
                    ",\"notDestroyed\":"
                } else {
                    ",\"destroyed\":"
                })
                .map_err(|_| Error::State)?;
                let mut first = true;
                let mut results = complete.results.iter();
                for input in &inputs {
                    let result = if input.local.is_some() {
                        *results.next().ok_or(Error::State)?
                    } else {
                        EmailDestroyResult::NotFound
                    };
                    if (result == EmailDestroyResult::Destroyed) == errors {
                        continue;
                    }
                    out.append(if first {
                        if errors {
                            "{"
                        } else {
                            "["
                        }
                    } else {
                        ","
                    })
                    .map_err(|_| Error::State)?;
                    first = false;
                    let id = std::str::from_utf8(input.name.bytes()).map_err(|_| Error::State)?;
                    out.format(format_args!("\"{id}\""))
                        .map_err(|_| Error::State)?;
                    if errors {
                        out.append(":{\"type\":\"notFound\"}")
                            .map_err(|_| Error::State)?;
                    }
                }
                out.append(if first {
                    "null"
                } else if errors {
                    "}"
                } else {
                    "]"
                })
                .map_err(|_| Error::State)?;
            }
            out.append("}").map_err(|_| Error::State)?;
            let length = out.len();
            self.retention
                .finish_mutation_output(reserved, output.get(..length).ok_or(Error::State)?)
                .map_err(Error::Storage)
        })();
        encoded.map_err(|error| match error {
            Error::State => Error::ServerPartialFail,
            other => other,
        })
    }
}
