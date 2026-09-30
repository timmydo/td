//! Bounded TLS wire progress over an exclusively owned transport.
//! Policy authorization and pool leases belong to the admitting factory.
use crate::ports::{Clock, Deadline, Error, FlushProgress, IoProgress, Transport};
use std::sync::Arc;
use td_crypto::{HandshakeInfo, TlsPhase, TlsProgress, TlsSession};

pub const TLS_WIRE_BYTES: usize = 18_437;
const PLAIN_BYTES: usize = 16 * 1024;

/// Construction has already aborted both handles. Recover these reservations
/// even when a pool owns only their moved references, not the original arrays.
pub struct TlsIoRefusal<'a> {
    error: Error,
    input: &'a mut [u8; TLS_WIRE_BYTES],
    output: &'a mut [u8; TLS_WIRE_BYTES],
}

impl<'a> TlsIoRefusal<'a> {
    pub const fn error(&self) -> Error {
        self.error
    }

    pub fn into_buffers(self) -> (&'a mut [u8; TLS_WIRE_BYTES], &'a mut [u8; TLS_WIRE_BYTES]) {
        (self.input, self.output)
    }
}

impl std::fmt::Debug for TlsIoRefusal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsIoRefusal")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

/// One connection and two caller-reserved ciphertext buffers. Raw handshake
/// evidence is not mail authorization; no gateway proof is constructed here.
pub struct TlsIo<'a, T: Transport> {
    transport: T,
    session: TlsSession,
    clock: Arc<dyn Clock>,
    deadline: Deadline,
    handshake_deadline: Deadline,
    input: Option<&'a mut [u8; TLS_WIRE_BYTES]>,
    input_used: usize,
    input_target: usize,
    output: Option<&'a mut [u8; TLS_WIRE_BYTES]>,
    output_start: usize,
    output_end: usize,
    output_target: usize,
    network_eof: bool,
    ready: bool,
    write_closed: bool,
    failure: Option<Error>,
}

impl<'a, T: Transport> TlsIo<'a, T> {
    /// Consume a handshaking session and transport. Deadlines are absolute and
    /// cannot be renewed. Refusal aborts both handles and returns both buffers.
    pub fn new(
        mut transport: T,
        mut session: TlsSession,
        clock: Arc<dyn Clock>,
        deadline: Deadline,
        handshake_deadline: Deadline,
        input: &'a mut [u8; TLS_WIRE_BYTES],
        output: &'a mut [u8; TLS_WIRE_BYTES],
    ) -> Result<Self, TlsIoRefusal<'a>> {
        let admitted =
            if session.status().phase != TlsPhase::Handshaking || handshake_deadline > deadline {
                Err(Error::Invalid)
            } else {
                check_deadlines(clock.as_ref(), deadline, Some(handshake_deadline))
            };
        if let Err(error) = admitted {
            session.abort();
            transport.abort();
            return Err(TlsIoRefusal {
                error,
                input,
                output,
            });
        }
        Ok(Self {
            transport,
            session,
            clock,
            deadline,
            handshake_deadline,
            input: Some(input),
            input_used: 0,
            input_target: 5,
            output: Some(output),
            output_start: 0,
            output_end: 0,
            output_target: 5,
            network_eof: false,
            ready: false,
            write_closed: false,
            failure: None,
        })
    }

    /// Progress at most one input and one output record. Finished is exposed
    /// only after the local handshake flight has drained through the transport.
    pub fn handshake(&mut self) -> Result<Option<HandshakeInfo>, Error> {
        self.run(|connection| {
            connection.step()?;
            if connection.session.status().handshake.is_some() && connection.flushed()? {
                connection.ready = true;
            }
            Ok(connection.evidence())
        })
    }

    /// Cached cryptographic evidence, cleared on local failure. The admitting
    /// owner must separately check current policy, gateway pins and peer address.
    pub fn evidence(&self) -> Option<HandshakeInfo> {
        if self.ready && self.failure.is_none() {
            self.session.status().handshake
        } else {
            None
        }
    }

    /// Abort and return the reserved buffers for another connection. Their
    /// bytes are not erased; only the next owner's logical lengths matter.
    pub fn into_buffers(
        mut self,
    ) -> Result<(&'a mut [u8; TLS_WIRE_BYTES], &'a mut [u8; TLS_WIRE_BYTES]), Error> {
        self.abort();
        let input = self.input.take().ok_or(Error::Invalid)?;
        let output = self.output.take().ok_or(Error::Invalid)?;
        Ok((input, output))
    }

    fn check_deadline(&self, handshaking: bool) -> Result<(), Error> {
        check_deadlines(
            self.clock.as_ref(),
            self.deadline,
            handshaking.then_some(self.handshake_deadline),
        )
    }

    fn fail(&mut self, error: Error) -> Error {
        let error = *self.failure.get_or_insert(error);
        self.session.abort();
        self.transport.abort();
        self.ready = false;
        self.input_used = 0;
        self.input_target = 5;
        self.output_start = 0;
        self.output_end = 0;
        self.output_target = 5;
        error
    }

    fn run<R>(&mut self, work: impl FnOnce(&mut Self) -> Result<R, Error>) -> Result<R, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let handshaking = !self.ready;
        let result = (|| {
            self.check_deadline(handshaking)?;
            // Refresh clock/key health even when only a retained socket tail moves.
            self.session
                .drain_ciphertext(&mut [])
                .map_err(|_| Error::Tls)?;
            let result = work(self)?;
            self.check_deadline(handshaking)?;
            self.session
                .drain_ciphertext(&mut [])
                .map_err(|_| Error::Tls)?;
            Ok(result)
        })();
        result.map_err(|error| self.fail(error))
    }

    fn step(&mut self) -> Result<(), Error> {
        self.send_record()?;
        self.receive_record()
    }

    fn flushed(&mut self) -> Result<bool, Error> {
        if self.output_end != 0 || self.session.status().ciphertext_pending != 0 {
            return Ok(false);
        }
        Ok(self.transport.flush()? == FlushProgress::Complete)
    }

    fn send_record(&mut self) -> Result<(), Error> {
        if self.output_end == 0 && self.session.status().ciphertext_pending == 0 {
            return Ok(());
        }
        if self.output_end < 5 {
            self.drain_output()?;
            if self.output_end < 5 {
                return Ok(());
            }
            self.output_target = record_length(self.output.as_deref().ok_or(Error::Invalid)?)?;
        }
        if self.output_end < self.output_target {
            self.drain_output()?;
            if self.output_end < self.output_target {
                return Ok(());
            }
        }
        let tail = self
            .output
            .as_deref()
            .ok_or(Error::Invalid)?
            .get(self.output_start..self.output_end)
            .ok_or(Error::Invalid)?;
        match self.transport.write(tail)? {
            IoProgress::Bytes(count) if count > 0 && count <= tail.len() => {
                self.output_start = self.output_start.checked_add(count).ok_or(Error::Invalid)?;
                if self.output_start == self.output_end {
                    self.output_start = 0;
                    self.output_end = 0;
                    self.output_target = 5;
                }
                Ok(())
            }
            IoProgress::Pending => Ok(()),
            _ => Err(Error::Invalid),
        }
    }

    fn drain_output(&mut self) -> Result<(), Error> {
        let remaining = self
            .output
            .as_deref_mut()
            .ok_or(Error::Invalid)?
            .get_mut(self.output_end..self.output_target)
            .ok_or(Error::Invalid)?;
        let count = self
            .session
            .drain_ciphertext(remaining)
            .map_err(|_| Error::Tls)?;
        if count == 0 || count > remaining.len() {
            return Err(Error::Tls);
        }
        self.output_end = self.output_end.checked_add(count).ok_or(Error::Invalid)?;
        Ok(())
    }

    fn receive_record(&mut self) -> Result<(), Error> {
        if self.network_eof || self.session.status().plaintext_pending != 0 {
            return Ok(());
        }
        if self.input_used < self.input_target {
            let remaining = self
                .input
                .as_deref_mut()
                .ok_or(Error::Invalid)?
                .get_mut(self.input_used..self.input_target)
                .ok_or(Error::Invalid)?;
            match self.transport.read(remaining)? {
                IoProgress::Bytes(count) if count > 0 && count <= remaining.len() => {
                    self.input_used = self.input_used.checked_add(count).ok_or(Error::Invalid)?;
                }
                IoProgress::Pending => return Ok(()),
                IoProgress::Closed => {
                    if self.input_used != 0 {
                        return Err(Error::Tls);
                    }
                    self.session.transport_eof().map_err(|_| Error::Tls)?;
                    self.network_eof = true;
                    return Ok(());
                }
                _ => return Err(Error::Invalid),
            }
            if self.input_used < self.input_target {
                return Ok(());
            }
            if self.input_target == 5 {
                self.input_target = record_length(self.input.as_deref().ok_or(Error::Invalid)?)?;
                if self.input_used < self.input_target {
                    return Ok(());
                }
            }
        }
        let wire = self
            .input
            .as_deref()
            .ok_or(Error::Invalid)?
            .get(..self.input_used)
            .ok_or(Error::Invalid)?;
        match self
            .session
            .receive_record(wire, self.output_end != 0)
            .map_err(|_| Error::Tls)?
        {
            TlsProgress::Bytes(count) if count == wire.len() => {
                self.input_used = 0;
                self.input_target = 5;
                Ok(())
            }
            TlsProgress::Blocked(_) => Ok(()),
            _ => Err(Error::Tls),
        }
    }
}

fn check_deadlines(
    clock: &dyn Clock,
    deadline: Deadline,
    handshake: Option<Deadline>,
) -> Result<(), Error> {
    let now = clock.sample()?.monotonic;
    if deadline.expired(now) || handshake.is_some_and(|cap| cap.expired(now)) {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}

fn record_length(header: &[u8]) -> Result<usize, Error> {
    let high = *header.get(3).ok_or(Error::Tls)?;
    let low = *header.get(4).ok_or(Error::Tls)?;
    let length = usize::from(u16::from_be_bytes([high, low]))
        .checked_add(5)
        .ok_or(Error::Tls)?;
    // Match the facade's strict outer bound, below its reserved capacity.
    if length >= TLS_WIRE_BYTES {
        return Err(Error::Tls);
    }
    Ok(length)
}

impl<T: Transport> Transport for TlsIo<'_, T> {
    fn read(&mut self, output: &mut [u8]) -> Result<IoProgress, Error> {
        if output.is_empty() {
            return Ok(IoProgress::Pending);
        }
        self.run(|connection| {
            connection.step()?;
            if !connection.ready {
                return Ok(IoProgress::Pending);
            }
            let count = output.len().min(PLAIN_BYTES);
            let output = output.get_mut(..count).ok_or(Error::Invalid)?;
            match connection
                .session
                .read_plaintext(output)
                .map_err(|_| Error::Tls)?
            {
                TlsProgress::Bytes(count) if count > 0 && count <= output.len() => {
                    Ok(IoProgress::Bytes(count))
                }
                TlsProgress::Blocked(_) => Ok(IoProgress::Pending),
                TlsProgress::Eof => Ok(IoProgress::Closed),
                _ => Err(Error::Tls),
            }
        })
    }

    fn write(&mut self, input: &[u8]) -> Result<IoProgress, Error> {
        if input.is_empty() {
            return Ok(IoProgress::Pending);
        }
        self.run(|connection| {
            if connection.session.status().write_closed {
                return Err(Error::Tls);
            }
            if !connection.ready
                || connection.output_end != 0
                || !connection.session.status().write_ready
            {
                connection.step()?;
                return Ok(IoProgress::Pending);
            }
            let input = input
                .get(..input.len().min(PLAIN_BYTES))
                .ok_or(Error::Invalid)?;
            match connection
                .session
                .queue_plaintext(input)
                .map_err(|_| Error::Tls)?
            {
                TlsProgress::Bytes(count) if count > 0 && count <= input.len() => {
                    connection.send_record()?;
                    Ok(IoProgress::Bytes(count))
                }
                TlsProgress::Blocked(_) => Ok(IoProgress::Pending),
                _ => Err(Error::Tls),
            }
        })
    }

    fn flush(&mut self) -> Result<FlushProgress, Error> {
        self.run(|connection| {
            connection.step()?;
            Ok(if connection.flushed()? {
                FlushProgress::Complete
            } else {
                FlushProgress::Pending
            })
        })
    }

    fn close(&mut self) -> Result<FlushProgress, Error> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.write_closed {
            return Ok(FlushProgress::Complete);
        }
        self.run(|connection| {
            connection.session.close().map_err(|_| Error::Tls)?;
            connection.step()?;
            if !connection.flushed()? {
                return Ok(FlushProgress::Pending);
            }
            if !connection.write_closed {
                if connection.transport.close()? == FlushProgress::Pending {
                    return Ok(FlushProgress::Pending);
                }
                connection.write_closed = true;
            }
            Ok(FlushProgress::Complete)
        })
    }

    fn abort(&mut self) {
        self.fail(Error::Invalid);
    }
}

impl<T: Transport> Drop for TlsIo<'_, T> {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
#[path = "tls_io_tests.rs"]
mod tests;
