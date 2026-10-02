//! Shared checked journal frames; input type determines whole-file or prefix completion.
use super::{input::fill_exact_using, CompleteFile, CompletePrefix, PrefixReader, StoreReader};
use crate::{
    format::{
        container::{Error as ContainerError, JournalHeader},
        frame::Frame,
        frame_header::Header as FrameHeader,
        journal_stream::{Error as StreamError, Summary, Verifier},
        Error as FormatError, Sequence, FRAME_HEADER_BYTES, JOURNAL_HEADER_BYTES, MAX_FRAME_BYTES,
    },
    ports::Crypto,
};
use std::io;
pub(super) const MAX_READ_CALLS: usize = 64;
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    Stream(StreamError),
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<StreamError> for Error {
    fn from(e: StreamError) -> Self {
        Self::Stream(e)
    }
}
impl From<ContainerError> for Error {
    fn from(e: ContainerError) -> Self {
        Self::Stream(e.into())
    }
}
impl From<FormatError> for Error {
    fn from(e: FormatError) -> Self {
        Self::Stream(e.into())
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "journal input I/O: {e}"),
            Self::Stream(e) => write!(f, "journal input validation: {e}"),
        }
    }
}
impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Stream(e) => Some(e),
        }
    }
}
pub(super) trait Input: Sized {
    type Complete;
    fn len(&self) -> u64;
    fn position(&self) -> u64;
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize>;
    fn finish(self) -> io::Result<Self::Complete>;
}
impl<'r> Input for StoreReader<'r> {
    type Complete = CompleteFile<'r>;
    fn len(&self) -> u64 {
        self.len()
    }
    fn position(&self) -> u64 {
        self.position()
    }
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read(output)
    }
    fn finish(self) -> io::Result<Self::Complete> {
        self.finish()
    }
}
impl<'r> Input for PrefixReader<'r> {
    type Complete = CompletePrefix<'r>;
    fn len(&self) -> u64 {
        self.len()
    }
    fn position(&self) -> u64 {
        self.position()
    }
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.read(output)
    }
    fn finish(self) -> io::Result<Self::Complete> {
        self.finish()
    }
}
pub(super) struct FrameInput<'c, 'b, C: Crypto, R: Input> {
    file: R,
    verifier: Verifier<'c, C>,
    crypto: &'c C,
    through: Sequence,
    scratch: &'b mut [u8; MAX_FRAME_BYTES],
    failed: bool,
}
impl<'c, 'b, C: Crypto, R: Input> FrameInput<'c, 'b, C, R> {
    pub(super) fn new(
        crypto: &'c C,
        mut file: R,
        expected: JournalHeader,
        scratch: &'b mut [u8; MAX_FRAME_BYTES],
    ) -> Result<Self, Error> {
        let mut bytes = [0; JOURNAL_HEADER_BYTES];
        let mut attempts = MAX_READ_CALLS;
        fill_exact_using(&mut file, &mut bytes, &mut attempts, Input::read)?;
        let verifier = Verifier::new(crypto, &bytes)?;
        if verifier.header() != expected {
            return Err(FormatError::InvalidValue.into());
        }
        Ok(Self {
            file,
            verifier,
            crypto,
            through: expected.base,
            scratch,
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
    /// One provisional frame; None is not physical EOF or selected completion.
    pub fn next_frame(&mut self) -> Result<Option<Frame<'_>>, Error> {
        self.next_frame_using(Input::read)
    }
    pub(super) fn next_frame_using(
        &mut self,
        mut read: impl FnMut(&mut R, &mut [u8]) -> io::Result<usize>,
    ) -> Result<Option<Frame<'_>>, Error> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        if self.file.position() == self.file.len() {
            return Ok(None);
        }
        self.failed = true;
        let remaining = self
            .file
            .len()
            .checked_sub(self.file.position())
            .ok_or(FormatError::InvalidValue)?;
        if remaining < FRAME_HEADER_BYTES as u64 {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        let prefix = self
            .scratch
            .get_mut(..FRAME_HEADER_BYTES)
            .ok_or(FormatError::Limit)?;
        let mut attempts = MAX_READ_CALLS;
        fill_exact_using(&mut self.file, prefix, &mut attempts, &mut read)?;
        let header =
            FrameHeader::decode(self.crypto, prefix).map_err(|e| StreamError::Frame(e.into()))?;
        if header.sequence != self.through.successor()? {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        if header.frame_bytes as u64 > remaining {
            return Err(StreamError::Frame(FormatError::InvalidValue.into()).into());
        }
        let bytes = self
            .scratch
            .get_mut(..header.frame_bytes)
            .ok_or(FormatError::Limit)?;
        fill_exact_using(
            &mut self.file,
            bytes
                .get_mut(FRAME_HEADER_BYTES..)
                .ok_or(FormatError::Limit)?,
            &mut attempts,
            &mut read,
        )?;
        let frame = self.verifier.push(bytes)?;
        self.through = frame.header().sequence;
        self.failed = false;
        Ok(Some(frame))
    }
    pub(super) fn finish(self) -> Result<(R::Complete, Summary), Error> {
        if self.failed {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe).into());
        }
        let summary = self.verifier.finish()?;
        let file = self.file.finish()?;
        Ok((file, summary))
    }
}
