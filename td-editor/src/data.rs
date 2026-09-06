//! Bounded core data-device v3 state and exact event decoding.

use crate::wire::{Cursor, Message};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const UTF8: &str = "text/plain;charset=utf-8";
pub(crate) const PLAIN: &str = "text/plain";
pub(crate) const OFFER_LIMIT: usize = 32;

#[derive(Default)]
pub(crate) struct Clipboard {
    pub manager: Option<(u32, u32)>, // registry name, client object
    pub device: Option<u32>,
    pub source: Option<(u32, Arc<str>)>,
    pub selection: Option<u32>,
    pub offers: BTreeMap<u32, Offer>,
    pub barriers: BTreeMap<u32, Vec<(u32, u64)>>,
    pub sequence: u64,
    pub incoming: Option<crate::transfer::Incoming>,
    pub incoming_target: Option<(crate::model::TabId, u64, crate::model::Selection)>,
    pub outgoing: Option<crate::transfer::Outgoing>,
}

pub(crate) struct Offer {
    pub sequence: u64,
    pub retired: bool,
    pub utf8: Option<String>,
    pub plain: Option<String>,
    pub count: usize,
}

impl Offer {
    pub fn mime(&self) -> Option<&str> {
        if self.retired {
            None
        } else {
            self.utf8.as_deref().or(self.plain.as_deref())
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum DeviceEvent {
    Offer(u32),
    Enter { surface: u32, offer: u32 },
    Selection(u32),
    Drag,
}

pub(crate) fn device(message: &Message) -> Result<DeviceEvent, String> {
    let mut c = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => DeviceEvent::Offer(c.u32()?),
        1 => {
            c.u32()?;
            let surface = c.u32()?;
            c.i32()?;
            c.i32()?;
            DeviceEvent::Enter {
                surface,
                offer: c.u32()?,
            }
        }
        2 | 4 => DeviceEvent::Drag,
        3 => {
            c.u32()?;
            c.i32()?;
            c.i32()?;
            DeviceEvent::Drag
        }
        5 => DeviceEvent::Selection(c.u32()?),
        _ => return Err("unknown data-device event".into()),
    };
    c.finish()?;
    Ok(event)
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum SourceEvent {
    Send(String),
    Cancel,
    Other,
}

fn mime(value: String) -> Result<String, String> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err("invalid clipboard MIME".into());
    }
    Ok(value)
}

fn action(value: u32) -> Result<(), String> {
    if !matches!(value, 0 | 1 | 2 | 4) {
        return Err("invalid data-device action".into());
    }
    Ok(())
}

pub(crate) fn source(message: &Message) -> Result<SourceEvent, String> {
    let mut c = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => {
            if let Some(value) = c.optional_string()? {
                mime(value)?;
            }
            SourceEvent::Other
        }
        1 => SourceEvent::Send(mime(c.string()?)?),
        2 => SourceEvent::Cancel,
        3 | 4 => SourceEvent::Other,
        5 => {
            action(c.u32()?)?;
            SourceEvent::Other
        }
        _ => return Err("unknown data-source event".into()),
    };
    c.finish()?;
    Ok(event)
}

pub(crate) fn offer(message: &Message) -> Result<Option<String>, String> {
    let mut c = Cursor::new(&message.payload);
    let result = match message.opcode {
        0 => Some(mime(c.string()?)?),
        1 => {
            if c.u32()? & !7 != 0 {
                return Err("invalid data-offer actions".into());
            }
            None
        }
        2 => {
            action(c.u32()?)?;
            None
        }
        _ => return Err("unknown data-offer event".into()),
    };
    c.finish()?;
    Ok(result)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::wire::{self, Builder};

    fn words(opcode: u16, words: &[u32]) -> Message {
        let mut body = Builder::new();
        for value in words {
            body.u32(*value);
        }
        wire::take(&mut body.message(10, opcode).unwrap())
            .unwrap()
            .unwrap()
    }

    fn text(opcode: u16, value: &str) -> Message {
        let mut body = Builder::new();
        body.string(value).unwrap();
        wire::take(&mut body.message(10, opcode).unwrap())
            .unwrap()
            .unwrap()
    }

    fn copy(message: &Message) -> Message {
        Message {
            object: message.object,
            opcode: message.opcode,
            payload: message.payload.clone(),
        }
    }

    #[test]
    fn every_v3_schema_rejects_truncation_extra_words_and_unknown_values() {
        let devices = [
            words(0, &[0xff00_0010]),
            words(1, &[1, 7, 0, 0, 0]),
            words(2, &[]),
            words(3, &[0, 0, 0]),
            words(4, &[]),
            words(5, &[0]),
        ];
        let sources = [
            words(0, &[0]),
            text(0, PLAIN),
            text(1, UTF8),
            words(2, &[]),
            words(3, &[]),
            words(4, &[]),
            words(5, &[4]),
        ];
        let offers = [text(0, PLAIN), words(1, &[7]), words(2, &[2])];
        for message in devices {
            assert!(device(&message).is_ok());
            let mut extra = copy(&message);
            extra.payload.extend_from_slice(&[0; 4]);
            assert!(device(&extra).is_err());
            for length in 0..message.payload.len() {
                let mut short = copy(&message);
                short.payload.truncate(length);
                assert!(device(&short).is_err());
            }
        }
        for message in sources {
            assert!(source(&message).is_ok());
            let mut extra = copy(&message);
            extra.payload.extend_from_slice(&[0; 4]);
            assert!(source(&extra).is_err());
            for length in 0..message.payload.len() {
                let mut short = copy(&message);
                short.payload.truncate(length);
                assert!(source(&short).is_err());
            }
        }
        for message in offers {
            assert!(offer(&message).is_ok());
            let mut extra = copy(&message);
            extra.payload.extend_from_slice(&[0; 4]);
            assert!(offer(&extra).is_err());
            for length in 0..message.payload.len() {
                let mut short = copy(&message);
                short.payload.truncate(length);
                assert!(offer(&short).is_err());
            }
        }
        assert!(device(&words(6, &[])).is_err());
        assert!(source(&words(6, &[])).is_err());
        assert!(offer(&words(3, &[])).is_err());
        assert!(offer(&words(1, &[8])).is_err());
        for value in [3, 5, 7, u32::MAX] {
            assert!(source(&words(5, &[value])).is_err());
            assert!(offer(&words(2, &[value])).is_err());
        }
        for value in ["".to_string(), "x\0y".into(), "x\ny".into()] {
            assert!(source(&text(1, &value)).is_err());
            assert!(offer(&text(0, &value)).is_err());
        }
        assert!(source(&text(1, &"x".repeat(257))).is_ok());
        assert!(offer(&text(0, &"x".repeat(257))).is_ok());
    }
}
