//! CTAP2 HID framing for the supported 64-byte report profile. No device I/O.

pub const REPORT_SIZE: usize = 64;
pub const MAX_MESSAGE: usize = 57 + 128 * 59;
pub const BROADCAST: u32 = u32::MAX;
const INIT: u8 = 0x86;
const CBOR: u8 = 0x90;
const CANCEL: u8 = 0x91;
const KEEPALIVE: u8 = 0xbb;
const ERROR: u8 = 0xbf;

/// Owned wire reports; dropping them best-effort clears their payload bytes.
pub struct Reports(Vec<[u8; REPORT_SIZE]>);

impl AsRef<[[u8; REPORT_SIZE]]> for Reports {
    fn as_ref(&self) -> &[[u8; REPORT_SIZE]] {
        &self.0
    }
}

impl Drop for Reports {
    fn drop(&mut self) {
        for report in &mut self.0 {
            report.fill(0);
        }
    }
}

#[derive(PartialEq, Eq)]
pub struct Message(Vec<u8>);

impl AsRef<[u8]> for Message {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Message {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "CTAP message ({} bytes)", self.0.len())
    }
}

impl Drop for Message {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub fn cbor(channel: u32, payload: &[u8]) -> Result<Reports, String> {
    encode(channel, Command::Cbor, payload)
}

pub fn cancel(channel: u32) -> Result<Reports, String> {
    encode(channel, Command::Cancel, &[])
}

/// Owns the same fresh kernel nonce for request encoding and reply matching.
/// Ignored reports never extend the transport's absolute deadline.
pub struct Initialization {
    nonce: [u8; 8],
    channel: u32,
    decoder: Decoder,
}

impl Initialization {
    pub fn new(channel: u32, nonce: [u8; 8]) -> Result<Self, String> {
        Ok(Self {
            nonce,
            channel,
            decoder: Decoder::new(channel, Command::Init)?,
        })
    }

    pub fn request(&self) -> Result<Reports, String> {
        encode(self.channel, Command::Init, &self.nonce)
    }

    pub fn push(&mut self, report: &[u8; REPORT_SIZE]) -> Result<Option<u32>, String> {
        match self.decoder.push(report)? {
            Event::Complete(bytes) => {
                let received = bytes.as_ref().get(..8).ok_or("short HID INIT nonce")?;
                if received != self.nonce {
                    self.decoder = Decoder::new(self.channel, Command::Init)?;
                    return Ok(None);
                }
                let channel = allocated_channel(bytes.as_ref(), &self.nonce)?;
                if self.channel != BROADCAST && channel != self.channel {
                    return Err("HID initialization changed the synchronized channel".into());
                }
                Ok(Some(channel))
            }
            Event::OtherChannel | Event::Pending => Ok(None),
            _ => Err("unexpected HID initialization event".into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Command {
    Init,
    Cbor,
    Cancel,
}

impl Command {
    fn wire(self) -> u8 {
        match self {
            Self::Init => INIT,
            Self::Cbor => CBOR,
            Self::Cancel => CANCEL,
        }
    }
}

fn put(report: &mut [u8], start: usize, bytes: &[u8]) -> Result<(), String> {
    let end = start
        .checked_add(bytes.len())
        .ok_or("HID report overflow")?;
    report
        .get_mut(start..end)
        .ok_or("HID report overflow")?
        .copy_from_slice(bytes);
    Ok(())
}

fn allocated(channel: u32) -> bool {
    channel != 0 && channel != BROADCAST
}

fn encode(channel: u32, command: Command, payload: &[u8]) -> Result<Reports, String> {
    let valid = match command {
        Command::Init => channel != 0 && payload.len() == 8,
        Command::Cbor => allocated(channel) && !payload.is_empty(),
        Command::Cancel => allocated(channel) && payload.is_empty(),
    };
    if !valid || payload.len() > MAX_MESSAGE {
        return Err("invalid CTAP HID request".into());
    }
    let size = u16::try_from(payload.len()).map_err(|_| "oversized CTAP HID request")?;
    let mut reports = Reports(Vec::with_capacity(
        1 + payload.len().saturating_sub(57).div_ceil(59),
    ));
    let mut first = [0; REPORT_SIZE];
    put(&mut first, 0, &channel.to_be_bytes())?;
    put(&mut first, 4, &[command.wire()])?;
    put(&mut first, 5, &size.to_be_bytes())?;
    let split = payload.len().min(57);
    put(
        &mut first,
        7,
        payload.get(..split).ok_or("short HID payload")?,
    )?;
    reports.0.push(first);
    first.fill(0);
    for (sequence, part) in payload
        .get(split..)
        .ok_or("short HID payload")?
        .chunks(59)
        .enumerate()
    {
        let sequence = u8::try_from(sequence).map_err(|_| "HID sequence overflow")?;
        if sequence > 127 {
            return Err("HID sequence overflow".into());
        }
        let mut next = [0; REPORT_SIZE];
        put(&mut next, 0, &channel.to_be_bytes())?;
        put(&mut next, 4, &[sequence])?;
        put(&mut next, 5, part)?;
        reports.0.push(next);
        next.fill(0);
    }
    Ok(reports)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    OtherChannel,
    Pending,
    Processing,
    WaitingForPresence,
    Complete(Message),
}

/// One response transaction. Keepalives never reset a caller's I/O deadline.
pub struct Decoder {
    channel: u32,
    command: Command,
    remaining: Option<usize>,
    sequence: u8,
    bytes: Message,
    closed: bool,
}

impl Decoder {
    pub fn cbor(channel: u32) -> Result<Self, String> {
        Self::new(channel, Command::Cbor)
    }

    fn new(channel: u32, command: Command) -> Result<Self, String> {
        if channel == 0
            || command == Command::Cancel
            || (command == Command::Cbor && !allocated(channel))
        {
            return Err("invalid CTAP HID response channel".into());
        }
        Ok(Self {
            channel,
            command,
            remaining: None,
            sequence: 0,
            bytes: Message(Vec::with_capacity(MAX_MESSAGE)),
            closed: false,
        })
    }

    pub fn push(&mut self, report: &[u8; REPORT_SIZE]) -> Result<Event, String> {
        if self.closed {
            return Err("CTAP HID transaction is closed".into());
        }
        // Every malformed report on this channel permanently poisons the transaction.
        self.closed = true;
        let event = match self.accept(report) {
            Ok(event) => event,
            Err(error) => {
                self.bytes.0.fill(0);
                return Err(error);
            }
        };
        self.closed = matches!(event, Event::Complete(_));
        Ok(event)
    }

    fn accept(&mut self, report: &[u8; REPORT_SIZE]) -> Result<Event, String> {
        let channel = u32::from_be_bytes(
            report
                .get(..4)
                .ok_or("short HID channel")?
                .try_into()
                .map_err(|_| "short HID channel")?,
        );
        if channel != self.channel {
            return Ok(Event::OtherChannel);
        }
        let command = *report.get(4).ok_or("short HID command")?;
        let data = if command & 0x80 != 0 {
            let size = usize::from(u16::from_be_bytes(
                report
                    .get(5..7)
                    .ok_or("short HID length")?
                    .try_into()
                    .map_err(|_| "short HID length")?,
            ));
            if command == ERROR {
                if size != 1 {
                    return Err("invalid CTAP HID error length".into());
                }
                let code = report.get(7).ok_or("short HID error")?;
                return Err(format!("CTAP HID device refused request: {code:#04x}"));
            }
            if self.remaining.is_some() {
                return Err("CTAP HID response restarted before completion".into());
            }
            if command == KEEPALIVE {
                if self.command != Command::Cbor || size != 1 {
                    return Err("invalid CTAP HID keepalive".into());
                }
                return match report.get(7) {
                    Some(1) => Ok(Event::Processing),
                    Some(2) => Ok(Event::WaitingForPresence),
                    _ => Err("invalid CTAP HID keepalive status".into()),
                };
            }
            if command != self.command.wire() || size == 0 || size > MAX_MESSAGE {
                return Err("invalid CTAP HID response command or length".into());
            }
            self.remaining = Some(size);
            report.get(7..).ok_or("short initial HID report")?
        } else {
            if self.remaining.is_none() || command != self.sequence {
                return Err("invalid CTAP HID continuation sequence".into());
            }
            self.sequence = self
                .sequence
                .checked_add(1)
                .ok_or("HID sequence overflow")?;
            report.get(5..).ok_or("short continuation HID report")?
        };
        let remaining = self.remaining.ok_or("missing HID response length")?;
        let count = remaining.min(data.len());
        self.bytes
            .0
            .extend_from_slice(data.get(..count).ok_or("short HID response data")?);
        self.remaining = Some(remaining - count);
        if count == remaining {
            Ok(Event::Complete(std::mem::replace(
                &mut self.bytes,
                Message(Vec::new()),
            )))
        } else {
            Ok(Event::Pending)
        }
    }
}

/// The nonce must be fresh kernel randomness selected before sending INIT.
fn allocated_channel(response: &[u8], nonce: &[u8; 8]) -> Result<u32, String> {
    // CTAP requires accepting future fields after the current 17-byte prefix.
    if response.len() < 17
        || response.len() > MAX_MESSAGE
        || response.get(..8) != Some(nonce.as_slice())
        || response.get(12) != Some(&2)
        || response.get(16).is_none_or(|flags| flags & 4 == 0)
    {
        return Err("invalid CTAP2 HID initialization response".into());
    }
    let channel = u32::from_be_bytes(
        response
            .get(8..12)
            .ok_or("short HID allocated channel")?
            .try_into()
            .map_err(|_| "short HID allocated channel")?,
    );
    if !allocated(channel) {
        return Err("invalid CTAP HID allocated channel".into());
    }
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_ignores_another_nonce_without_resending() {
        let mut init = Initialization::new(BROADCAST, [1; 8]).unwrap();
        assert_eq!(&init.request().unwrap().as_ref()[0][7..15], &[1; 8]);
        let mut response = [0u8; REPORT_SIZE];
        response[..7].copy_from_slice(&[255, 255, 255, 255, INIT, 0, 17]);
        response[7..15].fill(2);
        response[15..19].copy_from_slice(&7u32.to_be_bytes());
        response[19] = 2;
        response[23] = 4;
        assert_eq!(init.push(&response).unwrap(), None);
        response[7..15].fill(1);
        assert_eq!(init.push(&response).unwrap(), Some(7));
        assert!(init.push(&response).is_err());
    }

    #[test]
    fn resynchronization_cannot_change_an_existing_channel() {
        let mut response = [0u8; REPORT_SIZE];
        response[..7].copy_from_slice(&[0, 0, 0, 7, INIT, 0, 17]);
        response[7..15].fill(1);
        response[19] = 2;
        response[23] = 4;
        for returned in [7u32, 8] {
            response[15..19].copy_from_slice(&returned.to_be_bytes());
            let mut init = Initialization::new(7, [1; 8]).unwrap();
            assert_eq!(init.push(&response).is_ok(), returned == 7);
            assert!(init.push(&response).is_err());
        }
    }

    #[test]
    fn wire_vectors_boundaries_and_padding() {
        assert_eq!(
            &encode(BROADCAST, Command::Init, &[1; 8]).unwrap().0[0][..15],
            &[255, 255, 255, 255, 0x86, 0, 8, 1, 1, 1, 1, 1, 1, 1, 1]
        );
        for size in [1, 57, 58, 116, 117, MAX_MESSAGE] {
            let payload: Vec<_> = (0..size).map(|n| n as u8).collect();
            let reports = encode(7, Command::Cbor, &payload).unwrap().0.clone();
            let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
            for (index, report) in reports.iter().enumerate() {
                let event = decoder.push(report).unwrap();
                if index + 1 == reports.len() {
                    assert_eq!(event, Event::Complete(Message(payload.clone())));
                } else {
                    assert_eq!(event, Event::Pending);
                }
            }
            assert!(decoder.push(&reports[0]).is_err());
        }
        assert!(encode(7, Command::Cbor, &vec![0; MAX_MESSAGE + 1]).is_err());
        assert!(encode(BROADCAST, Command::Cbor, &[0]).is_err());
        assert!(encode(0, Command::Init, &[0; 8]).is_err());
        assert!(encode(7, Command::Cancel, &[0]).is_err());
        assert_eq!(
            encode(7, Command::Cancel, &[]).unwrap().0[0][4..7],
            [CANCEL, 0, 0]
        );
        assert!(Decoder::new(7, Command::Cancel).is_err());
        let mut report = [0xa5; REPORT_SIZE];
        report[..8].copy_from_slice(&[0, 0, 0, 7, CBOR, 0, 1, 0]);
        assert_eq!(
            Decoder::new(7, Command::Cbor)
                .unwrap()
                .push(&report)
                .unwrap(),
            Event::Complete(Message(vec![0]))
        );
    }

    #[test]
    fn reordered_or_restarted_messages_poison_the_transaction() {
        let reports = encode(7, Command::Cbor, &[0; 117]).unwrap().0.clone();
        for report in [reports[1], reports[2]] {
            let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
            assert!(decoder.push(&report).is_err());
            assert!(decoder.push(&reports[0]).is_err());
        }
        for next in [reports[0], reports[2]] {
            let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
            assert_eq!(decoder.push(&reports[0]).unwrap(), Event::Pending);
            assert!(decoder.push(&next).is_err());
            assert!(decoder.push(&reports[1]).is_err());
        }
        for prefix in [
            [CBOR, 0, 0],
            [CBOR, 0x1d, 0xba],
            [INIT, 0, 1],
            [ERROR, 0, 1],
        ] {
            let mut report = reports[0];
            report[4..7].copy_from_slice(&prefix);
            let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
            assert!(decoder.push(&report).is_err());
            assert!(decoder.push(&reports[0]).is_err());
        }
    }

    #[test]
    fn device_error_during_assembly_preserves_its_code_and_closes() {
        let reports = encode(7, Command::Cbor, &[7; 117]).unwrap().0.clone();
        let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
        assert_eq!(decoder.push(&reports[0]).unwrap(), Event::Pending);
        let mut error = reports[0];
        error[4..8].copy_from_slice(&[ERROR, 0, 1, 6]);
        assert!(decoder.bytes.as_ref().iter().any(|byte| *byte != 0));
        assert!(decoder.push(&error).unwrap_err().contains("0x06"));
        assert!(decoder.bytes.as_ref().iter().all(|byte| *byte == 0));
        assert!(decoder.push(&reports[1]).is_err());
    }

    #[test]
    fn keepalives_and_other_channels_are_not_completion() {
        let valid = encode(7, Command::Cbor, &[0]).unwrap().0[0];
        let mut decoder = Decoder::new(7, Command::Cbor).unwrap();
        let mut other = valid;
        other[3] = 8;
        assert_eq!(decoder.push(&other).unwrap(), Event::OtherChannel);
        let mut keepalive = valid;
        keepalive[4] = KEEPALIVE;
        for (status, event) in [(1, Event::Processing), (2, Event::WaitingForPresence)] {
            keepalive[7] = status;
            assert_eq!(decoder.push(&keepalive).unwrap(), event);
        }
        assert_eq!(
            decoder.push(&valid).unwrap(),
            Event::Complete(Message(vec![0]))
        );
        keepalive[7] = 3;
        assert!(Decoder::new(7, Command::Cbor)
            .unwrap()
            .push(&keepalive)
            .is_err());
    }

    #[test]
    fn initialization_binds_nonce_protocol_channel_and_cbor_support() {
        let mut response = vec![1; 17];
        response[8..12].copy_from_slice(&7u32.to_be_bytes());
        response[12] = 2;
        response[16] = 4;
        assert_eq!(allocated_channel(&response, &[1; 8]).unwrap(), 7);
        for index in [0, 7, 12, 16] {
            let mut bad = response.clone();
            bad[index] = 0;
            assert!(allocated_channel(&bad, &[1; 8]).is_err());
        }
        for channel in [0u32, BROADCAST] {
            let mut bad = response.clone();
            bad[8..12].copy_from_slice(&channel.to_be_bytes());
            assert!(allocated_channel(&bad, &[1; 8]).is_err());
        }
        for size in 0..17 {
            assert!(allocated_channel(&response[..size], &[1; 8]).is_err());
        }
        response.extend_from_slice(&[0; 64]);
        assert_eq!(allocated_channel(&response, &[1; 8]).unwrap(), 7);
    }
}
