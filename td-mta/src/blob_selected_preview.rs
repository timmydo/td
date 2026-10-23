//! Interpret and stream the selected preview under the original native owners.
use crate::{
    admission::work::{Charge, Stop},
    blob_body_lists, blob_part, blob_preview, blob_preview_selection as selection, blob_structure,
    mime_body_lists::{Media, Node, View},
    mime_input::Checkpoints,
    mime_part_headers::label_json,
    nfc::{Credit, Scratch},
    ports::{Clock, Tick},
    preview_json::Format,
};
pub use blob_preview::{Diagnostics, Progress, Status};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Metadata(blob_part::Error),
    Preview(blob_preview::Error),
    State,
    Backing,
    Work(Stop),
    Admission(crate::nfc::Error),
    Policy(crate::ports::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "selected native preview: {self:?}")
    }
}
impl std::error::Error for Error {}
pub struct Buffers<'w, 'i> {
    pub metadata: label_json::Backing<'w>,
    pub scratch: &'w mut Scratch,
    pub input: &'i mut [u8],
    pub checkpoints: &'i mut Checkpoints,
}
pub struct Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'i> {
    pub original: selection::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
    pub scratch: &'w mut Scratch,
    pub input: &'i mut [u8],
    pub checkpoints: &'i mut Checkpoints,
    /// None means that no textual body source was selected.
    pub diagnostics: Option<Diagnostics>,
}
// One inline slot reuses metadata storage for decoding without heap allocation.
#[allow(clippy::large_enum_variant)]
enum Stage<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't> {
    Empty(
        blob_structure::Retained<'p, 'v, 'c, 'b, 'o, 'w>,
        u8,
        &'w mut Scratch,
    ),
    Metadata(blob_part::Cursor<'p, 'v, 'c, 'b, 'o, 'w>),
    Preview(blob_preview::Cursor<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't>),
}
/// Bytes are provisional until successful finish. Each poll copies at most six.
pub struct Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'i, 't> {
    stage: Option<Stage<'p, 'v, 'c, 'b, 'o, 'w, 'i, 't>>,
    choice: Option<selection::Choice>,
    nodes: &'n [Node],
    lists: View<'w>,
    input: Option<&'i mut [u8]>,
    checkpoints: Option<&'i mut Checkpoints>,
    clock: &'t dyn Clock,
    failure: Option<Error>,
}
impl<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'i, 't> Cursor<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'i, 't> {
    /// Consume a freshly completed selector; passive fabricated choices are not authorized.
    pub fn new(
        original: selection::Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w>,
        buffers: Buffers<'w, 'i>,
        clock: &'t dyn Clock,
        now: Tick,
    ) -> Result<Self, Error> {
        let selection::Retained { original, choice } = original;
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        let valid = choice.is_none()
            || (!buffers.input.is_empty() && buffers.input.len() <= crate::mime_input::INPUT_BYTES);
        original
            .structure
            .source
            .check_deadline()
            .map_err(Error::Policy)?;
        if !valid {
            return Err(Error::Backing);
        }
        let stage = if let Some(choice) = choice {
            Stage::Metadata(
                blob_part::Cursor::new(
                    original.structure,
                    choice.ordinal,
                    buffers.metadata,
                    buffers.scratch,
                    now,
                )
                .map_err(Error::Metadata)?,
            )
        } else {
            Stage::Empty(original.structure, 0, buffers.scratch)
        };
        let mut cursor = Self {
            stage: Some(stage),
            choice,
            nodes: original.nodes,
            lists: original.lists,
            input: Some(buffers.input),
            checkpoints: Some(buffers.checkpoints),
            clock,
            failure: None,
        };
        cursor.check_deadline(now)?;
        Ok(cursor)
    }
    fn outcome<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn empty_charge(
        original: &mut blob_structure::Retained<'_, '_, '_, '_, '_, '_>,
        now: Tick,
        charge: Charge,
    ) -> Result<(), Error> {
        original.source.check_deadline().map_err(Error::Policy)?;
        let result = (|| {
            original
                .budget
                .charge(original.work, now, 0, 0, &mut Credit::new())
                .map_err(Error::Admission)?;
            original.work.charge(now, charge).map_err(Error::Work)
        })();
        original
            .source
            .check_deadline()
            .map_err(Error::Policy)
            .and(result)
    }
    pub fn check_deadline(&mut self, now: Tick) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = match self.stage.as_mut() {
            Some(Stage::Empty(original, _, _)) => {
                Self::empty_charge(original, now, Charge::default())
            }
            Some(Stage::Metadata(child)) => child.check_deadline(now).map_err(Error::Metadata),
            Some(Stage::Preview(child)) => child.check().map_err(Error::Preview),
            None => Err(Error::State),
        };
        self.outcome(result)
    }
    /// Some(None) is a completed empty preview; None is incomplete or failed.
    pub fn value(&self) -> Option<Option<Diagnostics>> {
        if self.failure.is_some() {
            return None;
        }
        match self.stage.as_ref()? {
            Stage::Empty(_, 2, _) => Some(None),
            Stage::Preview(child) => child.value().map(Some),
            _ => None,
        }
    }
    pub fn poll(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = self.step(now, output);
        self.outcome(result)
    }
    /// Batch only scalar decoding; metadata retains its existing bounded turns.
    pub fn poll_batch(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        self.check_deadline(now)?;
        let result = match self.stage.as_mut() {
            Some(Stage::Preview(child)) => child.poll_batch(output).map_err(Error::Preview),
            _ => self.step(now, output),
        };
        self.outcome(result)
    }
    fn step(&mut self, now: Tick, output: &mut [u8]) -> Result<Progress, Error> {
        match self.stage.as_mut().ok_or(Error::State)? {
            Stage::Empty(original, position, _) => {
                let status = if *position == 2 {
                    Status::Complete
                } else {
                    Status::NeedOutput
                };
                let Some(slot) = output.first_mut().filter(|_| *position < 2) else {
                    return Ok(Progress { written: 0, status });
                };
                Self::empty_charge(
                    original,
                    now,
                    Charge {
                        records: 1,
                        output_bytes: 1,
                        ..Charge::default()
                    },
                )?;
                *slot = b'"';
                *position += 1;
                Self::empty_charge(original, now, Charge::default())?;
                Ok(Progress {
                    written: 1,
                    status: if *position == 2 {
                        Status::Complete
                    } else {
                        Status::Yield
                    },
                })
            }
            Stage::Preview(child) => child.poll(output).map_err(Error::Preview),
            Stage::Metadata(child) => {
                if child.poll(now).map_err(Error::Metadata)? != blob_part::Status::Complete {
                    return Ok(Progress {
                        written: 0,
                        status: Status::Yield,
                    });
                }
                let Some(Stage::Metadata(child)) = self.stage.take() else {
                    return Err(Error::State);
                };
                let original = child.finish(now).map_err(Error::Metadata)?;
                let choice = self.choice.ok_or(Error::State)?;
                let index = usize::from(choice.ordinal)
                    .checked_sub(1)
                    .ok_or(Error::State)?;
                let valid = (|| {
                    original
                        .structure
                        .work
                        .charge(
                            now,
                            Charge {
                                records: 1,
                                ..Charge::default()
                            },
                        )
                        .map_err(Error::Work)?;
                    let spec = original
                        .text_spec()
                        .map_err(|e| Error::Metadata(e.into()))?;
                    Ok(original.metadata.part.ordinal == choice.ordinal
                        && self.nodes.get(index) == Some(&original.metadata.node)
                        && original.metadata.node.class.media
                            == match choice.format {
                                Format::Plain => Media::Plain,
                                Format::Html => Media::Html,
                            }
                        && (self.lists.text.binary_search(&choice.ordinal).is_ok()
                            || self.lists.html.binary_search(&choice.ordinal).is_ok())
                        && choice.format
                            == if spec.html {
                                Format::Html
                            } else {
                                Format::Plain
                            })
                })();
                original
                    .structure
                    .source
                    .check_deadline()
                    .map_err(Error::Policy)?;
                if !valid? {
                    return Err(Error::State);
                }
                let child = blob_preview::Cursor::new(
                    original,
                    self.input.take().ok_or(Error::State)?,
                    self.checkpoints.take().ok_or(Error::State)?,
                    self.clock,
                    now,
                )
                .map_err(Error::Preview)?;
                self.stage = Some(Stage::Preview(child));
                Ok(Progress {
                    written: 0,
                    status: Status::Yield,
                })
            }
        }
    }
    pub fn finish(mut self, now: Tick) -> Result<Retained<'p, 'v, 'c, 'b, 'o, 'n, 'w, 'i>, Error> {
        self.check_deadline(now)?;
        let diagnostics = self.value().ok_or(Error::State)?;
        let (structure, input, checkpoints, scratch) =
            match self.stage.take().ok_or(Error::State)? {
                Stage::Empty(original, 2, scratch) => (
                    original,
                    self.input.take().ok_or(Error::State)?,
                    self.checkpoints.take().ok_or(Error::State)?,
                    scratch,
                ),
                Stage::Preview(child) => {
                    let retained = child.finish().map_err(Error::Preview)?;
                    (
                        retained.original.structure,
                        retained.buffer,
                        retained.checkpoints,
                        retained.original.scratch,
                    )
                }
                _ => return Err(Error::State),
            };
        structure.source.check_deadline().map_err(Error::Policy)?;
        Ok(Retained {
            original: selection::Retained {
                original: blob_body_lists::Retained {
                    structure,
                    nodes: self.nodes,
                    lists: self.lists,
                },
                choice: self.choice,
            },
            input,
            checkpoints,
            diagnostics,
            scratch,
        })
    }
}
const _: () = assert!(std::mem::size_of::<Cursor<'_, '_, '_, '_, '_, '_, '_, '_, '_>>() <= 8192);
#[cfg(test)]
#[path = "blob_selected_preview_tests.rs"]
mod tests;
