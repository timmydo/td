//! Shared operation input; whole-file or captured-prefix reader defines completion.
use super::{
    input::fill_exact_using,
    journal_input::{Error as InputError, Input},
};
use crate::{
    format::{
        container::JournalHeader,
        frame::DecodeError,
        journal_stream::{changes::Verifier, Error as StreamError, Summary},
        operation,
        table::MAX_RECORD_BYTES,
        Error as FormatError, FRAME_FOOTER_BYTES, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES,
        MAX_FRAME_OPERATIONS, OPERATION_HEADER_BYTES,
    },
    frame_changes::{Cell, CompleteChanges},
    ports::Crypto,
};
use std::io;
pub(super) const MAX_FRAME_READS: usize = 2 * MAX_FRAME_OPERATIONS + 66;

pub(super) struct ChangesInput<'c, 'b, C: Crypto, R: Input> {
    file: R,
    verifier: Verifier<'c, C>,
    available: Option<&'b mut [Cell]>,
    complete: Option<CompleteChanges<'b>>,
    failed: bool,
}
fn invalid_frame(error: FormatError) -> InputError {
    StreamError::Frame(DecodeError::Invalid(error.into())).into()
}
impl<'c, 'b, C: Crypto, R: Input> ChangesInput<'c, 'b, C, R> {
    pub fn new(
        crypto: &'c C,
        mut file: R,
        expected: JournalHeader,
        cells: &'b mut [Cell],
    ) -> Result<Self, InputError> {
        let mut bytes = [0; JOURNAL_HEADER_BYTES];
        let mut attempts = super::journal_input::MAX_READ_CALLS;
        fill_exact_using(&mut file, &mut bytes, &mut attempts, R::read)?;
        let verifier = Verifier::new(crypto, &bytes)?;
        if verifier.header() != expected {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(Self {
            file,
            verifier,
            available: Some(cells),
            complete: None,
            failed: false,
        })
    }
    #[cfg(test)]
    pub(super) fn position(&self) -> u64 {
        self.file.position()
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    /// Locally checked changes only; selected completion and final-view proof remain pending.
    pub fn frame(&self) -> Option<&CompleteChanges<'_>> {
        if self.failed {
            None
        } else {
            self.complete.as_ref()
        }
    }
    /// Discards the previous frame, then reads one bounded frame. False is not EOF proof.
    pub fn advance_frame(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
    ) -> Result<bool, InputError> {
        self.advance_using(scratch, R::read)
    }
    pub(super) fn advance_using(
        &mut self,
        scratch: &mut [u8; MAX_RECORD_BYTES],
        mut read: impl FnMut(&mut R, &mut [u8]) -> io::Result<usize>,
    ) -> Result<bool, InputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        self.failed = true;
        if let Some(previous) = self.complete.take() {
            self.available = Some(previous.into_cells());
        }
        let remaining = self
            .file
            .len()
            .checked_sub(self.file.position())
            .ok_or_else(|| invalid_frame(FormatError::InvalidValue))?;
        if remaining == 0 {
            self.failed = false;
            return Ok(false);
        }
        if remaining < FRAME_HEADER_BYTES as u64 {
            return Err(invalid_frame(FormatError::InvalidValue));
        }
        let mut attempts = MAX_FRAME_READS;
        let mut header = [0; FRAME_HEADER_BYTES];
        fill_exact_using(&mut self.file, &mut header, &mut attempts, &mut read)?;
        let cells = self
            .available
            .take()
            .ok_or_else(|| invalid_frame(FormatError::InvalidValue))?;
        let mut pending = self.verifier.begin(&header, cells)?;
        let header = pending.header();
        if header.frame_bytes as u64 > remaining {
            return Err(invalid_frame(FormatError::InvalidValue));
        }
        let mut payload = header.payload_bytes().map_err(invalid_frame)?;
        for _ in 0..header.operations {
            if payload < OPERATION_HEADER_BYTES {
                return Err(invalid_frame(FormatError::InvalidValue));
            }
            let prefix = scratch
                .get_mut(..OPERATION_HEADER_BYTES)
                .ok_or_else(|| invalid_frame(FormatError::Limit))?;
            fill_exact_using(&mut self.file, prefix, &mut attempts, &mut read)?;
            let length = operation::extent(prefix).map_err(invalid_frame)?;
            payload = payload
                .checked_sub(length)
                .ok_or_else(|| invalid_frame(FormatError::InvalidValue))?;
            let bytes = scratch
                .get_mut(..length)
                .ok_or_else(|| invalid_frame(FormatError::Limit))?;
            fill_exact_using(
                &mut self.file,
                bytes
                    .get_mut(OPERATION_HEADER_BYTES..)
                    .ok_or_else(|| invalid_frame(FormatError::Limit))?,
                &mut attempts,
                &mut read,
            )?;
            pending.push(bytes)?;
        }
        if payload != 0 {
            return Err(invalid_frame(FormatError::InvalidValue));
        }
        let mut footer = [0; FRAME_FOOTER_BYTES];
        fill_exact_using(&mut self.file, &mut footer, &mut attempts, &mut read)?;
        self.complete = Some(pending.finish(&footer)?);
        self.failed = false;
        Ok(true)
    }
    pub fn finish_reuse(self) -> Result<(R::Complete, Summary, &'b mut [Cell]), InputError> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        let summary = self.verifier.finish()?;
        let file = self.file.finish()?;
        let cells = match self.complete {
            Some(complete) => complete.into_cells(),
            None => self
                .available
                .ok_or_else(|| invalid_frame(FormatError::InvalidValue))?,
        };
        Ok((file, summary, cells))
    }
}
