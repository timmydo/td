//! Kernel CPU notifications invalidate an observation, including round trips
//! which leave the online mask unchanged. Lost notifications do the same.
use crate::sys;

const MAX_DATAGRAMS: usize = 256;
const DATAGRAM_BYTES: usize = 4096;

pub struct Monitor {
    socket: sys::UeventSocket,
    sequence: Sequence,
    buffer: Vec<u8>,
}

impl Monitor {
    pub fn open() -> Result<Self, String> {
        Ok(Self {
            socket: sys::UeventSocket::open()
                .map_err(|e| format!("open kernel CPU notification stream: {e}"))?,
            sequence: Sequence::default(),
            buffer: vec![0; DATAGRAM_BYTES],
        })
    }

    pub fn check(&mut self) -> Result<(), String> {
        check_stream(&mut self.sequence, &mut self.buffer, |buffer| {
            self.socket.receive(buffer)
        })
    }
}

fn check_stream(
    sequence: &mut Sequence,
    buffer: &mut [u8],
    mut receive: impl FnMut(&mut [u8]) -> std::io::Result<sys::UeventRead>,
) -> Result<(), String> {
    for _ in 0..MAX_DATAGRAMS {
        match receive(buffer) {
            Ok(sys::UeventRead::Empty) => return Ok(()),
            Ok(sys::UeventRead::Foreign) => continue,
            Ok(sys::UeventRead::Kernel(length)) => {
                let bytes = buffer
                    .get(..length)
                    .ok_or("oversized kernel notification")?;
                sequence.observe(bytes)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("kernel CPU notification loss: {error}")),
        }
    }
    Err("kernel CPU notification drain budget exhausted".into())
}

#[derive(Default)]
struct Sequence(Option<u64>);

impl Sequence {
    fn observe(&mut self, bytes: &[u8]) -> Result<(), String> {
        let event = parse(bytes)?;
        if self
            .0
            .is_some_and(|prior| prior.checked_add(1) != Some(event.sequence))
        {
            return Err(
                "kernel notification sequence discontinuity; CPU topology uncertain".into(),
            );
        }
        self.0 = Some(event.sequence);
        if event.cpu {
            return Err("kernel CPU topology notification; restart observation".into());
        }
        Ok(())
    }
}

struct Notification {
    sequence: u64,
    cpu: bool,
}

fn parse(bytes: &[u8]) -> Result<Notification, String> {
    let body = bytes
        .strip_suffix(&[0])
        .ok_or("unterminated kernel notification")?;
    let mut fields = body.split(|byte| *byte == 0);
    let header = fields.next().ok_or("missing kernel notification header")?;
    let mut action = None;
    let mut path = None;
    let mut subsystem = None;
    let mut sequence = None;
    for field in fields {
        let equal = field
            .iter()
            .position(|byte| *byte == b'=')
            .ok_or("malformed kernel notification field")?;
        let key = field.get(..equal).ok_or("missing notification key")?;
        let value = field.get(equal + 1..).ok_or("missing notification value")?;
        let slot = match key {
            b"ACTION" => &mut action,
            b"DEVPATH" => &mut path,
            b"SUBSYSTEM" => &mut subsystem,
            b"SEQNUM" => &mut sequence,
            _ => continue,
        };
        if value.is_empty() || slot.replace(value).is_some() {
            return Err("empty or duplicate kernel notification identity".into());
        }
    }
    let action = action.ok_or("missing notification action")?;
    let path = path.ok_or("missing notification path")?;
    if header
        .strip_prefix(action)
        .and_then(|tail| tail.strip_prefix(b"@"))
        != Some(path)
    {
        return Err("kernel notification header disagrees with identity".into());
    }
    let subsystem = subsystem.ok_or("missing notification subsystem")?;
    let sequence = sequence.ok_or("missing notification sequence")?;
    if !sequence.iter().all(u8::is_ascii_digit) {
        return Err("invalid kernel notification sequence".into());
    }
    let sequence = std::str::from_utf8(sequence)
        .map_err(|_| "invalid sequence encoding")?
        .parse::<u64>()
        .map_err(|_| "kernel notification sequence overflow")?;
    if sequence == 0 {
        return Err("zero kernel notification sequence".into());
    }
    Ok(Notification {
        sequence,
        cpu: subsystem == b"cpu",
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn event(sequence: u64, action: &str, subsystem: &str) -> Vec<u8> {
        format!("{action}@/devices/system/cpu/cpu1\0ACTION={action}\0DEVPATH=/devices/system/cpu/cpu1\0SUBSYSTEM={subsystem}\0SEQNUM={sequence}\0").into_bytes()
    }

    #[test]
    fn offline_online_round_trip_cannot_hide_behind_an_unchanged_mask() {
        for action in ["online", "offline", "add", "remove", "change"] {
            let mut sequence = Sequence::default();
            sequence.observe(&event(10, "change", "block")).unwrap();
            assert!(sequence
                .observe(&event(11, action, "cpu"))
                .unwrap_err()
                .contains("CPU topology"));
        }
    }

    #[test]
    fn unrelated_events_keep_sequence_and_loss_is_not_silently_ignored() {
        let mut sequence = Sequence::default();
        sequence.observe(&event(10, "change", "block")).unwrap();
        sequence.observe(&event(11, "change", "block")).unwrap();
        assert!(sequence.observe(&event(13, "change", "block")).is_err());
        let mut sequence = Sequence(Some(u64::MAX));
        assert!(sequence.observe(&event(1, "change", "block")).is_err());
    }

    #[test]
    fn receiver_loss_and_flooding_require_a_fresh_observation() {
        let mut sequence = Sequence::default();
        let mut buffer = [0; DATAGRAM_BYTES];
        assert!(check_stream(&mut sequence, &mut buffer, |_| {
            Err(std::io::Error::from_raw_os_error(105))
        })
        .unwrap_err()
        .contains("notification loss"));
        let mut calls = 0;
        assert!(check_stream(&mut sequence, &mut buffer, |_| {
            calls += 1;
            Ok(sys::UeventRead::Foreign)
        })
        .unwrap_err()
        .contains("budget"));
        assert_eq!(calls, MAX_DATAGRAMS);
        check_stream(&mut sequence, &mut buffer, |_| Ok(sys::UeventRead::Empty)).unwrap();
        assert!(check_stream(&mut sequence, &mut buffer, |_| {
            Ok(sys::UeventRead::Kernel(DATAGRAM_BYTES + 1))
        })
        .is_err());
    }

    #[test]
    fn malformed_or_incomplete_notifications_refuse_observation() {
        let good = event(4, "online", "cpu");
        for length in 0..good.len() {
            assert!(parse(good.get(..length).unwrap()).is_err());
        }
        for bytes in [
            b"online@/a\0ACTION=offline\0DEVPATH=/a\0SUBSYSTEM=cpu\0SEQNUM=1\0".as_slice(),
            b"online@/a\0ACTION=online\0DEVPATH=/a\0SUBSYSTEM=cpu\0SEQNUM=1\0SEQNUM=2\0",
            b"online@/a\0ACTION=online\0DEVPATH=/a\0SUBSYSTEM=cpu\0SEQNUM=18446744073709551616\0",
            b"online@/a\0ACTION=online\0DEVPATH=/a\0SUBSYSTEM=cpu\0SEQNUM=0\0",
        ] {
            assert!(parse(bytes).is_err());
        }
        assert!(parse(&good).unwrap().cpu);
    }
}
