//! Complete SMTP replies normalized for bounded persisted reply cells.
use super::{Progress, ReplyReader};
use crate::{format::row::MAX_SMTP_REPLY, ports::Error};

/// No allocation or partial-result exposure. The first normalized line remains
/// the prefix reference for every later line, as required by RFC 8621 section 7.
/// The trusted driver still binds this reader to one command on one transport.
pub struct NormalizedReplyReader<'a> {
    reply: ReplyReader<'a>,
    output: &'a mut [u8],
    used: usize,
    first: usize,
    failure: Option<Error>,
}
impl<'a> NormalizedReplyReader<'a> {
    pub fn new(line: &'a mut [u8], output: &'a mut [u8]) -> Result<Self, Error> {
        Ok(Self {
            reply: ReplyReader::new(line)?,
            output: output.get_mut(..MAX_SMTP_REPLY).ok_or(Error::Capacity)?,
            used: 0,
            first: 0,
            failure: None,
        })
    }
    /// Consume through the final CRLF only. Preserve the unconsumed tail for
    /// the transport owner; continuation lines are handled within this reply.
    pub fn feed(&mut self, input: &[u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let result = self.feed_inner(input);
        if let Err(error) = result {
            self.failure = Some(error);
        }
        result
    }
    fn feed_inner(&mut self, input: &[u8]) -> Result<Progress, Error> {
        let mut consumed = 0;
        if self.reply.complete() {
            return Ok(Progress {
                consumed,
                complete: true,
            });
        }
        while consumed < input.len() {
            let progress = self
                .reply
                .feed(input.get(consumed..).ok_or(Error::Invalid)?)?;
            consumed = consumed
                .checked_add(progress.consumed)
                .ok_or(Error::Capacity)?;
            if !progress.complete {
                break;
            }
            self.append_line()?;
            if self.reply.complete() {
                return Ok(Progress {
                    consumed,
                    complete: true,
                });
            }
            self.reply.advance()?;
        }
        Ok(Progress {
            consumed,
            complete: false,
        })
    }
    fn append_line(&mut self) -> Result<(), Error> {
        let line = self.reply.line.line().ok_or(Error::Invalid)?;
        let normalized = |index: usize, byte: u8| {
            if index == 3 && byte == b'-' {
                b' '
            } else {
                byte
            }
        };
        let common = if self.first == 0 {
            0
        } else {
            self.output
                .get(..self.first)
                .ok_or(Error::Invalid)?
                .iter()
                .zip(line.iter().enumerate())
                .take_while(|(first, (index, byte))| **first == normalized(*index, **byte))
                .count()
        };
        let separator = usize::from(self.first != 0);
        let needed = line
            .len()
            .checked_sub(common)
            .and_then(|n| n.checked_add(separator))
            .and_then(|n| self.used.checked_add(n))
            .ok_or(Error::Capacity)?;
        if needed > self.output.len() {
            return Err(Error::Capacity);
        }
        if separator != 0 {
            *self.output.get_mut(self.used).ok_or(Error::Capacity)? = b' ';
            self.used += 1;
        }
        for (index, &byte) in line.iter().enumerate().skip(common) {
            *self.output.get_mut(self.used).ok_or(Error::Capacity)? = normalized(index, byte);
            self.used += 1;
        }
        if self.first == 0 {
            self.first = self.used;
        }
        Ok(())
    }
    pub fn code(&self) -> Option<u16> {
        if self.failure.is_some() || !self.reply.complete() {
            return None;
        }
        self.reply.line().map(|line| line.code)
    }
    pub fn value(&self) -> Option<&str> {
        self.code()?;
        std::str::from_utf8(self.output.get(..self.used)?).ok()
    }
    /// EOF before a complete reply is sticky refusal, never a successful prefix.
    pub fn finish(&mut self) -> Result<&str, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.reply.complete() {
            self.failure = Some(Error::Invalid);
            return Err(Error::Invalid);
        }
        self.value().ok_or(Error::Invalid)
    }
}

#[cfg(test)]
#[path = "smtp_wire_reply_tests.rs"]
mod tests;
