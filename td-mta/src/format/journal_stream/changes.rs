//! Incremental supplied journals with checked, compact per-frame changes.
use super::Error;
pub use super::Verifier;
use crate::{
    format::{container::Error as ContainerError, frame::DecodeError, Error as FormatError},
    frame_changes::{Cell, Collector, CompleteChanges},
    ports::{Crypto, Digest},
};

fn collector_error(error: ContainerError) -> Error {
    if error == ContainerError::Format(FormatError::OutputFull) {
        Error::Journal(error)
    } else {
        Error::Frame(DecodeError::Invalid(error))
    }
}

impl<'c, C: Crypto> Verifier<'c, C> {
    /// Dropping an unfinished frame permanently fails this journal.
    pub fn begin<'p, 's>(
        &'p mut self,
        header: &[u8],
        cells: &'s mut [Cell],
    ) -> Result<Pending<'p, 'c, 's, C>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        self.failed = Some(FormatError::Truncated.into());
        match self.prepare(header, cells) {
            Ok((collector, frame_bytes, operations)) => Ok(Pending {
                journal: self,
                collector,
                frame_bytes,
                operations,
                failed: None,
            }),
            Err(error) => {
                self.failed = Some(error);
                Err(error)
            }
        }
    }
    fn prepare<'s>(
        &mut self,
        header: &[u8],
        cells: &'s mut [Cell],
    ) -> Result<(Collector<'c, 's, C>, usize, usize), Error> {
        self.check_frame_start()?;
        let collector =
            Collector::new(self.crypto, self.through, header, cells).map_err(collector_error)?;
        let (frame_bytes, operations) = self.admit_header(collector.header())?;
        self.digest.update(header)?;
        Ok((collector, frame_bytes, operations))
    }
}

/// Exclusive in-progress frame. Only successful finish advances the parent journal.
pub struct Pending<'p, 'c, 's, C: Crypto> {
    journal: &'p mut Verifier<'c, C>,
    collector: Collector<'c, 's, C>,
    frame_bytes: usize,
    operations: usize,
    failed: Option<Error>,
}
impl<'s, C: Crypto> Pending<'_, '_, 's, C> {
    /// Checked header only; the caller must still supply and validate the payload/footer.
    pub const fn header(&self) -> crate::format::frame_header::Header {
        self.collector.header()
    }
    pub fn push(&mut self, operation: &[u8]) -> Result<(), Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let result = self
            .collector
            .push(operation)
            .map_err(collector_error)
            .and_then(|()| self.journal.digest.update(operation).map_err(Error::from));
        if let Err(error) = result {
            self.failed = Some(error);
            self.journal.failed = Some(error);
        }
        result
    }
    pub fn finish(self, footer: &[u8]) -> Result<CompleteChanges<'s>, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        let complete = match self.collector.finish(footer).map_err(collector_error) {
            Ok(complete) => complete,
            Err(error) => {
                self.journal.failed = Some(error);
                return Err(error);
            }
        };
        if let Err(error) = self.journal.digest.update(footer).map_err(Error::from) {
            self.journal.failed = Some(error);
            return Err(error);
        }
        self.journal.through = complete.summary().header().sequence;
        self.journal.frame_bytes = self.frame_bytes;
        self.journal.operations = self.operations;
        self.journal.failed = None;
        Ok(complete)
    }
}
