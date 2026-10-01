//! Bounded decoding for the seat's two selections and the offer record
//! the client keeps per server-created offer: exact schemas for the core
//! data-device v3 and primary-selection v1 device, source and offer
//! events, each board's request and event opcodes, the two text MIME
//! spellings a consumer offers and accepts, and the budgets on offers and
//! their announcements. No display, descriptor, environment or clock is
//! accessed.

use crate::wire::{Cursor, Message};

/// The explicit UTF-8 text MIME, preferred when an offer carries both.
pub const UTF8: &str = "text/plain;charset=utf-8";
/// Plain text, accepted as UTF-8 only.
pub const PLAIN: &str = "text/plain";
/// Retained offers, live and retired, at most.
pub const OFFER_LIMIT: usize = 32;
/// MIME announcements inspected per offer; later ones are drained.
pub const ANNOUNCEMENTS: usize = 64;
/// The longest MIME announcement retained.
pub const MIME_BYTES: usize = 256;

/// Which of the seat's two selections a data device serves. The two
/// protocols share their shape -- a manager, one device per seat, server
/// offers announcing MIMEs, and sources whose sends carry a right -- and
/// differ in opcodes and in the core device's drag events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Board {
    /// The core data device's selection, which copy and paste use.
    Clipboard,
    /// `zwp_primary_selection_device_manager_v1`'s, which selecting text
    /// sets and a middle click pastes.
    Primary,
}

impl Board {
    /// The board's name in a diagnostic.
    pub fn name(self) -> &'static str {
        match self {
            Board::Clipboard => "clipboard",
            Board::Primary => "primary selection",
        }
    }

    /// The other board, whose server offers share this one's id space.
    pub fn other(self) -> Board {
        match self {
            Board::Clipboard => Board::Primary,
            Board::Primary => Board::Clipboard,
        }
    }

    /// The manager's interface and the version bound, the only one read.
    pub fn global(self) -> (&'static str, u32) {
        match self {
            Board::Clipboard => ("wl_data_device_manager", 3),
            Board::Primary => ("zwp_primary_selection_device_manager_v1", 1),
        }
    }

    /// The device's `set_selection`.
    pub fn set_selection(self) -> u16 {
        match self {
            Board::Clipboard => 1,
            Board::Primary => 0,
        }
    }

    /// The device's destructor: the core device's `release`, the primary
    /// device's `destroy`.
    pub fn release(self) -> u16 {
        match self {
            Board::Clipboard => 2,
            Board::Primary => 1,
        }
    }

    /// An offer's `receive`.
    pub fn receive(self) -> u16 {
        match self {
            Board::Clipboard => 1,
            Board::Primary => 0,
        }
    }

    /// An offer's destructor.
    pub fn destroy_offer(self) -> u16 {
        match self {
            Board::Clipboard => 2,
            Board::Primary => 1,
        }
    }

    /// A source's destructor, the same on both.
    pub fn destroy_source(self) -> u16 {
        1
    }

    /// A source's `send` event, the one that carries a right.
    pub fn send(self) -> u16 {
        match self {
            Board::Clipboard => 1,
            Board::Primary => 0,
        }
    }

    /// A device event under this board's schema.
    pub fn device(self, message: &Message) -> Result<DeviceEvent, String> {
        match self {
            Board::Clipboard => device(message),
            Board::Primary => primary_device(message),
        }
    }

    /// A source event under this board's schema.
    pub fn source(self, message: &Message) -> Result<SourceEvent, String> {
        match self {
            Board::Clipboard => source(message),
            Board::Primary => primary_source(message),
        }
    }

    /// An offer event under this board's schema: its MIME, if it
    /// announced one.
    pub fn offer(self, message: &Message) -> Result<Option<String>, String> {
        match self {
            Board::Clipboard => offer(message),
            Board::Primary => primary_offer(message),
        }
    }
}

/// One server-created offer: its generation, whether it was destroyed
/// (and waits for its barrier), the exact spellings of the two supported
/// MIMEs it announced, and how many announcements were inspected.
#[derive(Debug, Eq, PartialEq)]
pub struct Offer {
    pub sequence: u64,
    pub retired: bool,
    pub utf8: Option<String>,
    pub plain: Option<String>,
    pub count: usize,
}

impl Offer {
    /// The MIME to receive with, the explicit UTF-8 spelling preferred;
    /// none for a retired offer or one without a supported text type.
    pub fn mime(&self) -> Option<&str> {
        if self.retired {
            None
        } else {
            self.utf8.as_deref().or(self.plain.as_deref())
        }
    }

    /// Records one announcement under the budgets: at most
    /// `ANNOUNCEMENTS` inspected, longer ones and later duplicates ignored,
    /// nothing retained on a retired offer.
    pub fn announce(&mut self, mime: String) {
        if self.count >= ANNOUNCEMENTS {
            return;
        }
        self.count += 1;
        if mime.len() > MIME_BYTES || self.retired {
            return;
        }
        if mime.eq_ignore_ascii_case(UTF8) && self.utf8.is_none() {
            self.utf8 = Some(mime);
        } else if mime.eq_ignore_ascii_case(PLAIN) && self.plain.is_none() {
            self.plain = Some(mime);
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum DeviceEvent {
    Offer(u32),
    Enter { surface: u32, offer: u32 },
    Selection(u32),
    Drag,
}

pub fn device(message: &Message) -> Result<DeviceEvent, String> {
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
pub enum SourceEvent {
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
        return Err("invalid data action".into());
    }
    Ok(())
}

pub fn source(message: &Message) -> Result<SourceEvent, String> {
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

pub fn offer(message: &Message) -> Result<Option<String>, String> {
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

/// `zwp_primary_selection_device_v1`: an offer, then the selection.
pub fn primary_device(message: &Message) -> Result<DeviceEvent, String> {
    let mut c = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => DeviceEvent::Offer(c.u32()?),
        1 => DeviceEvent::Selection(c.u32()?),
        _ => return Err("unknown primary-selection device event".into()),
    };
    c.finish()?;
    Ok(event)
}

/// `zwp_primary_selection_source_v1`: a send, or the cancel.
pub fn primary_source(message: &Message) -> Result<SourceEvent, String> {
    let mut c = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => SourceEvent::Send(mime(c.string()?)?),
        1 => SourceEvent::Cancel,
        _ => return Err("unknown primary-selection source event".into()),
    };
    c.finish()?;
    Ok(event)
}

/// `zwp_primary_selection_offer_v1`: a MIME announcement, its only event.
pub fn primary_offer(message: &Message) -> Result<Option<String>, String> {
    let mut c = Cursor::new(&message.payload);
    let result = match message.opcode {
        0 => Some(mime(c.string()?)?),
        _ => return Err("unknown primary-selection offer event".into()),
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

    #[test]
    fn every_primary_schema_rejects_truncation_extra_words_and_unknown_values() {
        let board = Board::Primary;
        let devices = [words(0, &[0xff00_0010]), words(1, &[0])];
        for message in devices {
            assert!(board.device(&message).is_ok());
            let mut extra = copy(&message);
            extra.payload.extend_from_slice(&[0; 4]);
            assert!(board.device(&extra).is_err());
            for length in 0..message.payload.len() {
                let mut short = copy(&message);
                short.payload.truncate(length);
                assert!(board.device(&short).is_err());
            }
        }
        assert_eq!(
            board.device(&words(1, &[7])).unwrap(),
            DeviceEvent::Selection(7)
        );
        for message in [text(0, UTF8), words(1, &[])] {
            assert!(board.source(&message).is_ok());
            let mut extra = copy(&message);
            extra.payload.extend_from_slice(&[0; 4]);
            assert!(board.source(&extra).is_err());
            for length in 0..message.payload.len() {
                let mut short = copy(&message);
                short.payload.truncate(length);
                assert!(board.source(&short).is_err());
            }
        }
        assert_eq!(
            board.source(&text(0, UTF8)).unwrap(),
            SourceEvent::Send(UTF8.into())
        );
        assert_eq!(board.source(&words(1, &[])).unwrap(), SourceEvent::Cancel);
        assert_eq!(board.offer(&text(0, PLAIN)).unwrap(), Some(PLAIN.into()));
        for opcode in [2, 3, 5] {
            assert!(board.device(&words(opcode, &[0])).is_err());
            assert!(board.source(&words(opcode, &[])).is_err());
        }
        // The core offer's action events are not the primary offer's.
        assert!(board.offer(&words(1, &[7])).is_err());
        for value in ["", "x\0y", "x\ny"] {
            assert!(board.source(&text(0, value)).is_err());
            assert!(board.offer(&text(0, value)).is_err());
        }
        // The two boards' opcodes differ where their protocols do.
        let opcodes = |b: Board| {
            (
                b.set_selection(),
                b.release(),
                b.receive(),
                b.destroy_offer(),
                b.destroy_source(),
                b.send(),
            )
        };
        assert_eq!(opcodes(Board::Clipboard), (1, 2, 1, 2, 1, 1));
        assert_eq!(opcodes(Board::Primary), (0, 1, 0, 1, 1, 0));
        assert_eq!(Board::Clipboard.other(), Board::Primary);
        assert_eq!(Board::Primary.other(), Board::Clipboard);
    }

    #[test]
    fn announcements_are_budgeted_and_the_explicit_utf8_spelling_is_preferred() {
        let mut offer = Offer {
            sequence: 1,
            retired: false,
            utf8: None,
            plain: None,
            count: 0,
        };
        offer.announce("x".repeat(MIME_BYTES + 1));
        offer.announce("Text/Plain".into());
        offer.announce(PLAIN.into());
        assert_eq!((offer.mime(), offer.count), (Some("Text/Plain"), 3));
        for _ in 0..ANNOUNCEMENTS {
            offer.announce("image/png".into());
        }
        assert_eq!(offer.count, ANNOUNCEMENTS);
        offer.announce("TEXT/PLAIN;CHARSET=UTF-8".into());
        assert_eq!(offer.mime(), Some("Text/Plain"), "over budget: ignored");
        let mut offer = Offer {
            sequence: 2,
            retired: false,
            utf8: None,
            plain: None,
            count: 0,
        };
        offer.announce(PLAIN.into());
        offer.announce("TEXT/PLAIN;CHARSET=UTF-8".into());
        assert_eq!(offer.mime(), Some("TEXT/PLAIN;CHARSET=UTF-8"));
        offer.retired = true;
        assert_eq!(offer.mime(), None);
        offer.announce(UTF8.into());
        assert_eq!(offer.count, 3, "retired offers drain their announcements");
    }
}
