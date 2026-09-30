//! Bounded SMTP control lines. DATA framing and protocol state live elsewhere.
use crate::ports::Error;

/// RFC 5321 control-line limit, including CRLF. Extensions may need other limits.
pub const LINE_BYTES: usize = 512;
/// Maximum aggregate wire bytes consumed for one reply, independent of storage.
pub const REPLY_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    /// Exact prefix consumed; the caller retains all bytes after it.
    pub consumed: usize,
    pub complete: bool,
}

/// Caller-owned storage; no allocation, compaction or implicit next-line parsing.
pub struct LineReader<'a> {
    storage: &'a mut [u8],
    used: usize,
    cr: bool,
    complete: bool,
    failure: Option<Error>,
}
impl<'a> LineReader<'a> {
    pub fn new(storage: &'a mut [u8]) -> Result<Self, Error> {
        let storage = storage.get_mut(..LINE_BYTES).ok_or(Error::Capacity)?;
        Ok(Self {
            storage,
            used: 0,
            cr: false,
            complete: false,
            failure: None,
        })
    }

    pub fn feed(&mut self, input: &[u8]) -> Result<Progress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.complete {
            return Ok(Progress {
                consumed: 0,
                complete: true,
            });
        }
        for (offset, &byte) in input.iter().enumerate() {
            let result = self.byte(byte);
            if let Err(error) = result {
                self.failure = Some(error);
                return Err(error);
            }
            if self.complete {
                return Ok(Progress {
                    consumed: offset + 1,
                    complete: true,
                });
            }
        }
        Ok(Progress {
            consumed: input.len(),
            complete: false,
        })
    }

    fn byte(&mut self, byte: u8) -> Result<(), Error> {
        if self.cr {
            if byte != b'\n' {
                return Err(Error::Invalid);
            }
            self.complete = true;
            return Ok(());
        }
        if byte == b'\r' {
            self.cr = true;
            return Ok(());
        }
        if byte == b'\n' || !(byte == b'\t' || (32..=126).contains(&byte)) {
            return Err(Error::Invalid);
        }
        if self.used >= LINE_BYTES - 2 {
            return Err(Error::Capacity);
        }
        *self.storage.get_mut(self.used).ok_or(Error::Capacity)? = byte;
        self.used += 1;
        Ok(())
    }

    /// Complete line without CRLF; failure never exposes partial bytes.
    pub fn line(&self) -> Option<&[u8]> {
        if !self.complete || self.failure.is_some() {
            return None;
        }
        self.storage.get(..self.used)
    }

    /// Explicitly retire a complete line. A framing failure cannot be reset.
    pub fn advance(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.complete {
            return Err(Error::Conflict);
        }
        self.storage.fill(0);
        self.used = 0;
        self.cr = false;
        self.complete = false;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplyLine<'a> {
    pub code: u16,
    pub last: bool,
    pub text: &'a [u8],
}
impl<'a> ReplyLine<'a> {
    /// Parse one already-framed line. A bare final code is valid SMTP.
    pub fn parse(line: &'a [u8]) -> Result<Self, Error> {
        let (digits, rest) = line.split_at_checked(3).ok_or(Error::Invalid)?;
        let [a @ b'2'..=b'5', b @ b'0'..=b'5', c @ b'0'..=b'9'] = digits else {
            return Err(Error::Invalid);
        };
        let (last, text) = match rest.split_first() {
            None => (true, rest),
            Some((b' ', text)) => (true, text),
            Some((b'-', text)) => (false, text),
            _ => return Err(Error::Invalid),
        };
        if line.len() > LINE_BYTES - 2
            || !text.iter().all(|&b| b == b'\t' || (32..=126).contains(&b))
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            code: u16::from(a - b'0') * 100 + u16::from(b - b'0') * 10 + u16::from(c - b'0'),
            last,
            text,
        })
    }
}

/// One SMTP reply. Lines retain the same code and share a fixed wire-byte cap.
/// Inspect each completed line before advance; do not act on a partial reply.
pub struct ReplyReader<'a> {
    line: LineReader<'a>,
    code: Option<u16>,
    bytes: usize,
    lines: usize,
    counted: bool,
    done: bool,
    failure: Option<Error>,
}
impl<'a> ReplyReader<'a> {
    pub fn new(storage: &'a mut [u8]) -> Result<Self, Error> {
        Ok(Self {
            line: LineReader::new(storage)?,
            code: None,
            bytes: 0,
            lines: 0,
            counted: false,
            done: false,
            failure: None,
        })
    }

    /// Stops at each CRLF, including continuation lines. Remaining bytes stay owned
    /// by the caller, even when they could contain another reply or TLS records.
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
        if self.counted {
            return Ok(Progress {
                consumed: 0,
                complete: true,
            });
        }
        let available = REPLY_BYTES.checked_sub(self.bytes).ok_or(Error::Capacity)?;
        let prefix = input
            .get(..input.len().min(available))
            .ok_or(Error::Invalid)?;
        let progress = self.line.feed(prefix)?;
        self.bytes = self
            .bytes
            .checked_add(progress.consumed)
            .ok_or(Error::Capacity)?;
        if progress.complete {
            let reply = ReplyLine::parse(self.line.line().ok_or(Error::Invalid)?)?;
            if self.code.is_some_and(|code| code != reply.code) {
                return Err(Error::Invalid);
            }
            self.code = Some(reply.code);
            self.lines = self.lines.checked_add(1).ok_or(Error::Capacity)?;
            self.done = reply.last;
            if !self.done && self.bytes == REPLY_BYTES {
                return Err(Error::Capacity);
            }
            self.counted = true;
        } else if self.bytes == REPLY_BYTES {
            return Err(Error::Capacity);
        }
        Ok(progress)
    }

    pub fn line(&self) -> Option<ReplyLine<'_>> {
        if !self.counted || self.failure.is_some() {
            return None;
        }
        ReplyLine::parse(self.line.line()?).ok()
    }

    pub fn first_line(&self) -> bool {
        self.counted && self.lines == 1 && self.failure.is_none()
    }

    pub fn complete(&self) -> bool {
        self.done && self.failure.is_none()
    }

    pub fn wire_bytes(&self) -> usize {
        self.bytes
    }

    /// Advance only within this reply; completion is deliberately terminal.
    pub fn advance(&mut self) -> Result<(), Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if !self.counted || self.done {
            return Err(Error::Conflict);
        }
        self.line.advance()?;
        self.counted = false;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EhloExtension<'a> {
    pub keyword: &'a [u8],
    pub parameters: &'a [u8],
}

/// A complete 250 EHLO reply advertised parameterless STARTTLS. This is syntax
/// evidence only; the trusted driver binds the reader and tail to its socket.
pub struct StartTlsOffer {
    _private: (),
}

/// Retains only the STARTTLS capability, never credentials or other EHLO state.
pub struct EhloReader<'a> {
    reply: ReplyReader<'a>,
    offered: bool,
}
impl<'a> EhloReader<'a> {
    pub fn new(storage: &'a mut [u8]) -> Result<Self, Error> {
        Ok(Self {
            reply: ReplyReader::new(storage)?,
            offered: false,
        })
    }

    pub fn feed(&mut self, input: &[u8]) -> Result<Progress, Error> {
        let progress = self.reply.feed(input)?;
        if let Some(line) = self.reply.line() {
            if let Ok(Some(extension)) = ehlo_extension(line, self.reply.first_line()) {
                self.offered |= extension.keyword.eq_ignore_ascii_case(b"STARTTLS")
                    && extension.parameters.is_empty();
            }
        }
        Ok(progress)
    }

    pub fn advance(&mut self) -> Result<(), Error> {
        self.reply.advance()
    }

    pub fn complete(&self) -> bool {
        self.reply.complete()
    }

    /// Consume pre-TLS capability state. A greeting, partial reply, malformed
    /// capability or unread plaintext tail cannot mint an offer.
    pub fn into_starttls_offer(self, tail: &[u8]) -> Result<StartTlsOffer, Error> {
        if let Some(error) = self.reply.failure {
            return Err(error);
        }
        if !tail.is_empty() {
            return Err(Error::Invalid);
        }
        if !self.reply.complete() {
            return Err(Error::Conflict);
        }
        if !self.offered || self.reply.line().is_none_or(|line| line.code != 250) {
            return Err(Error::Tls);
        }
        Ok(StartTlsOffer { _private: () })
    }
}

/// EHLO extension syntax from RFC 5321. The first greeting line is not an
/// extension. Callers retain recognized extensions only after the full 250 reply.
/// Invalid extension syntax is local to this line: skip it as unadvertised.
/// This error does not invalidate the enclosing, correctly framed SMTP reply.
pub fn ehlo_extension<'a>(
    line: ReplyLine<'a>,
    first: bool,
) -> Result<Option<EhloExtension<'a>>, Error> {
    if first || line.code != 250 {
        return Ok(None);
    }
    let (keyword, parameters) = match line.text.iter().position(|&b| b == b' ') {
        Some(index) => {
            let (keyword, rest) = line.text.split_at_checked(index).ok_or(Error::Invalid)?;
            let (_, parameters) = rest.split_first().ok_or(Error::Invalid)?;
            if parameters.is_empty() || parameters.split(|&b| b == b' ').any(|part| part.is_empty())
            {
                return Err(Error::Invalid);
            }
            (keyword, parameters)
        }
        None => (line.text, &[][..]),
    };
    let (first, rest) = keyword.split_first().ok_or(Error::Invalid)?;
    if !first.is_ascii_alphanumeric()
        || !rest.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        || !parameters.iter().all(|&b| (32..=126).contains(&b))
    {
        return Err(Error::Invalid);
    }
    Ok(Some(EhloExtension {
        keyword,
        parameters,
    }))
}

#[cfg(test)]
#[path = "smtp_wire_tests.rs"]
mod tests;
