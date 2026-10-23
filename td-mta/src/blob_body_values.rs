//! Stream a selected native bodyValues map with one borrowed value child at a time.
use crate::{
    admission::work::Charge,
    blob_body_value, blob_part, blob_structure, blob_value_parts,
    bounded::TextBuffer,
    mime_input::Checkpoints,
    mime_part_headers::{self, label_json},
    mime_response::bound,
    nfc::{Credit, Scratch},
    ports::{Clock, Error as PolicyError, Tick},
};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    State,
    Admission(crate::nfc::Error),
    Policy(PolicyError),
    Metadata(blob_part::Error),
    Value(blob_body_value::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native bodyValues: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Yield,
    NeedOutput,
    NeedValue(u16),
    Complete,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    pub written: usize,
    pub status: Status,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Head,
    Key,
    Value,
    Busy,
    End,
    Complete,
}
struct Control {
    phase: Phase,
    index: usize,
    position: usize,
    key_len: usize,
    failure: Option<Error>,
}
pub struct Buffers<'s> {
    pub metadata: label_json::Backing<'s>,
    pub scratch: &'s mut Scratch,
    pub input: &'s mut [u8],
    pub checkpoints: &'s mut Checkpoints,
}
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> {
    original: blob_value_parts::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's>,
    control: Control,
    key: [u8; 9],
    credit: Credit,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's> {
    pub fn new(
        original: blob_value_parts::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's>,
        now: Tick,
    ) -> Result<Self, Error> {
        let mut cursor = Self {
            original,
            control: Control {
                phase: Phase::Head,
                index: 0,
                position: 0,
                key_len: 0,
                failure: None,
            },
            key: [0; 9],
            credit: Credit::new(),
        };
        cursor.check_deadline(now)?;
        Ok(cursor)
    }
    fn charge(&mut self, now: Tick, steps: u64, bytes: u64) -> Result<(), Error> {
        let structure = &mut self.original.original.structure;
        structure
            .budget
            .charge(structure.work, now, 0, steps, &mut self.credit)
            .map_err(Error::Admission)?;
        structure
            .work
            .charge(
                now,
                Charge {
                    records: steps,
                    output_bytes: bytes,
                    ..Charge::default()
                },
            )
            .map_err(|e| Error::Admission(crate::nfc::Error::Work(e)))
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.control.failure = Some(error);
        }
        result
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.control.failure {
            return Err(error);
        }
        let result = (|| {
            self.original
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            let result = self.charge(now, 0, 0).and_then(|()| {
                if self.control.phase == Phase::Busy {
                    Err(Error::State)
                } else {
                    Ok(())
                }
            });
            self.original
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)
                .and(result)
        })();
        self.outcome(result)
    }
    pub fn value(&self) -> Option<()> {
        (self.control.failure.is_none() && self.control.phase == Phase::Complete).then_some(())
    }
    fn ordinal(&self) -> Result<u16, Error> {
        let ordinal = *self
            .original
            .ordinals
            .get(self.control.index)
            .ok_or(Error::State)?;
        if ordinal == 0
            || self
                .control
                .index
                .checked_sub(1)
                .and_then(|i| self.original.ordinals.get(i))
                .is_some_and(|previous| *previous >= ordinal)
        {
            return Err(Error::State);
        }
        Ok(ordinal)
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = self.step(now, output);
        let result = self
            .original
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result);
        self.outcome(result)
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        let status = match self.control.phase {
            Phase::Complete => Some(Status::Complete),
            Phase::Value => Some(Status::NeedValue(self.ordinal()?)),
            Phase::Busy => return Err(Error::State),
            _ => None,
        };
        if let Some(status) = status {
            return Ok(Progress { written: 0, status });
        }
        if output.is_empty() {
            return Ok(Progress {
                written: 0,
                status: Status::NeedOutput,
            });
        }
        self.charge(now, 1, 0)?;
        if self.control.phase == Phase::Key && self.control.key_len == 0 {
            let ordinal = self.ordinal()?;
            let mut key = TextBuffer::new(&mut self.key);
            key.format(format_args!(
                "{}\"{}\":",
                if self.control.index == 0 { "" } else { "," },
                ordinal
            ))
            .map_err(|_| Error::State)?;
            self.control.key_len = key.len();
        }
        let bytes = self.framing_bytes()?;
        let count = bytes
            .len()
            .checked_sub(self.control.position)
            .ok_or(Error::State)?
            .min(output.len())
            .min(64);
        let end = self
            .control
            .position
            .checked_add(count)
            .ok_or(Error::State)?;
        // Charge before borrowing the source slice again for the bounded copy.
        self.charge(now, 0, u64::try_from(count).map_err(|_| Error::State)?)?;
        let bytes = self.framing_bytes()?;
        output
            .get_mut(..count)
            .ok_or(Error::State)?
            .copy_from_slice(bytes.get(self.control.position..end).ok_or(Error::State)?);
        let complete = end == bytes.len();
        self.control.position = end;
        if complete {
            self.control.position = 0;
            self.control.phase = match self.control.phase {
                Phase::Head => {
                    if self.original.ordinals.is_empty() {
                        Phase::End
                    } else {
                        Phase::Key
                    }
                }
                Phase::Key => Phase::Value,
                Phase::End => Phase::Complete,
                _ => return Err(Error::State),
            };
        }
        Ok(Progress {
            written: count,
            status: if self.control.phase == Phase::Complete {
                Status::Complete
            } else {
                Status::Yield
            },
        })
    }
    fn framing_bytes(&self) -> Result<&[u8], Error> {
        match self.control.phase {
            Phase::Head => Ok(b"\"bodyValues\":{"),
            Phase::Key => self.key.get(..self.control.key_len).ok_or(Error::State),
            Phase::End => Ok(b"}"),
            _ => Err(Error::State),
        }
    }
    pub fn begin_value<'a>(
        &'a mut self,
        buffers: Buffers<'a>,
        clock: &'a dyn Clock,
        now: Tick,
    ) -> Result<Value<'a, 'v, 'c>, Error> {
        self.check_deadline(now)?;
        let prepared = (|| {
            if self.control.phase != Phase::Value {
                return Err(Error::State);
            }
            let ordinal = self.ordinal()?;
            let fact = *self
                .original
                .facts
                .get(usize::from(ordinal).checked_sub(1).ok_or(Error::State)?)
                .ok_or(Error::State)?;
            if fact.end.part.ordinal != ordinal || fact.kind == blob_value_parts::Kind::Other {
                return Err(Error::State);
            }
            Ok((ordinal, fact))
        })();
        let prepared = self
            .original
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(prepared);
        let (ordinal, fact) = self.outcome(prepared)?;
        self.control.phase = Phase::Busy;
        let structure = &mut self.original.original.structure;
        let original = blob_structure::Retained {
            source: &mut *structure.source,
            work: &mut *structure.work,
            budget: &mut *structure.budget,
            parts: structure.parts,
            headers: structure.headers,
            remaining: &mut *structure.remaining,
            header_bytes: structure.header_bytes,
        };
        let metadata =
            blob_part::Cursor::new(original, ordinal, buffers.metadata, buffers.scratch, now)
                .map_err(Error::Metadata);
        match metadata {
            Err(error) => {
                self.control.failure = Some(error);
                Err(error)
            }
            Ok(metadata) => Ok(Value {
                control: &mut self.control,
                metadata: Some(metadata),
                encoded: None,
                input: Some(buffers.input),
                checkpoints: Some(buffers.checkpoints),
                clock,
                fact,
                count: self.original.ordinals.len(),
                max_bytes: self.original.options.max_bytes,
            }),
        }
    }
    pub fn finish(
        mut self,
        now: Tick,
    ) -> Result<blob_value_parts::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'f, 's>, Error> {
        self.check_deadline(now)?;
        let complete = self.value().is_some();
        self.original
            .original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        if !complete {
            return Err(Error::State);
        }
        Ok(self.original)
    }
}
/// Forgetting or dropping this child before finish leaves the parent busy.
pub struct Value<'a, 'v, 'c> {
    control: &'a mut Control,
    metadata: Option<blob_part::Cursor<'a, 'v, 'c, 'a, 'a, 'a>>,
    encoded: Option<blob_body_value::Cursor<'a, 'v, 'c, 'a, 'a, 'a, 'a, 'a>>,
    input: Option<&'a mut [u8]>,
    checkpoints: Option<&'a mut Checkpoints>,
    clock: &'a dyn Clock,
    fact: blob_value_parts::Fact,
    count: usize,
    max_bytes: u64,
}
impl Value<'_, '_, '_> {
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.control.failure = Some(error);
            self.metadata = None;
            self.encoded = None;
        }
        result
    }
    pub fn value(&self) -> Option<blob_body_value::Diagnostics> {
        if self.control.failure.is_some() {
            None
        } else {
            self.encoded.as_ref()?.value()
        }
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.control.failure {
            return Err(error);
        }
        let result = if let Some(metadata) = self.metadata.as_mut() {
            metadata.check_deadline(now).map_err(Error::Metadata)
        } else if let Some(encoded) = self.encoded.as_mut() {
            encoded.check().map_err(Error::Value)
        } else {
            Err(Error::State)
        };
        self.outcome(result)
    }
    pub fn poll(
        &mut self,
        now: Tick,
        output: &mut [u8],
    ) -> Result<blob_body_value::Progress, Error> {
        self.check_deadline(now)?;
        let result = self.step(now, output);
        self.outcome(result)
    }
    /// Batch scalar decoding after metadata has completed.
    pub fn poll_batch(
        &mut self,
        now: Tick,
        output: &mut [u8],
    ) -> Result<blob_body_value::Progress, Error> {
        self.check_deadline(now)?;
        let result = if self.metadata.is_some() {
            self.step(now, output)
        } else if let Some(encoded) = self.encoded.as_mut() {
            encoded.poll_batch(output).map_err(Error::Value)
        } else {
            Err(Error::State)
        };
        self.outcome(result)
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<blob_body_value::Progress, Error> {
        if let Some(child) = self.metadata.as_mut() {
            if child.poll(now).map_err(Error::Metadata)? == mime_part_headers::Status::Complete {
                let metadata = self
                    .metadata
                    .take()
                    .ok_or(Error::State)?
                    .finish(now)
                    .map_err(Error::Metadata)?;
                let valid = metadata.metadata.part == self.fact.end.part
                    && metadata.metadata.node == self.fact.end.node
                    && blob_value_parts::Kind::from_headers(&metadata.metadata.metadata.headers)
                        == self.fact.kind;
                metadata
                    .structure
                    .source
                    .check_deadline()
                    .map_err(Error::Policy)?;
                if !valid {
                    return Err(Error::Metadata(blob_part::Error::Interpretation(
                        bound::Error::InvalidState,
                    )));
                }
                self.encoded = Some(
                    blob_body_value::Cursor::new(
                        metadata,
                        self.input.take().ok_or(Error::State)?,
                        self.checkpoints.take().ok_or(Error::State)?,
                        self.max_bytes,
                        self.clock,
                        now,
                    )
                    .map_err(Error::Value)?,
                );
            }
            return Ok(blob_body_value::Progress {
                written: 0,
                status: blob_body_value::Status::Yield,
            });
        }
        self.encoded
            .as_mut()
            .ok_or(Error::State)?
            .poll(output)
            .map_err(Error::Value)
    }
    pub fn finish(mut self, now: Tick) -> Result<blob_body_value::Diagnostics, Error> {
        self.check_deadline(now)?;
        let result = (|| {
            if let Some(metadata) = self.metadata.take() {
                // The unfinished child's finish supplies its native postcheck
                // and typed refusal. Never substitute a success handoff here.
                let retained = metadata.finish(now).map_err(Error::Metadata)?;
                retained
                    .structure
                    .source
                    .check_deadline()
                    .map_err(Error::Policy)?;
                return Err(Error::State);
            }
            let encoded = self
                .encoded
                .take()
                .ok_or(Error::State)?
                .finish()
                .map_err(Error::Value)?;
            let next = self.control.index.checked_add(1).ok_or(Error::State)?;
            if next > self.count {
                return Err(Error::State);
            }
            encoded
                .original
                .structure
                .source
                .check_deadline()
                .map_err(Error::Policy)?;
            self.control.index = next;
            self.control.position = 0;
            self.control.key_len = 0;
            self.control.phase = if next == self.count {
                Phase::End
            } else {
                Phase::Key
            };
            Ok(encoded.diagnostics)
        })();
        self.outcome(result)
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 1024);
const _: () = assert!(std::mem::size_of::<Value<'_, '_, '_>>() <= 8192);

#[cfg(test)]
#[path = "blob_body_values_tests.rs"]
mod tests;
