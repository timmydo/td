//! Safe control framing and read-only controller queries. No listener or I/O.

use crate::model::TabId;
use crate::ui::Controller;
use crate::{Error, Result};

pub const MAX_FRAME: usize = 1024 * 1024;
pub const PAGE_BYTES: usize = 256 * 1024;

/// One length-prefixed frame. Any refusal poisons it and drops partial text.
#[derive(Default)]
pub struct Decoder {
    header: [u8; 4],
    header_used: usize,
    payload: Vec<u8>,
    used: usize,
    failed: bool,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        let result = self.append(bytes);
        if result.is_err() {
            self.failed = true;
            self.payload = Vec::new();
        }
        result
    }

    fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        if self.failed {
            return Err(Error::Protocol);
        }
        if self.header_used < 4 {
            let take = bytes.len().min(4 - self.header_used);
            self.header
                .get_mut(self.header_used..self.header_used + take)
                .ok_or(Error::Protocol)?
                .copy_from_slice(bytes.get(..take).ok_or(Error::Protocol)?);
            self.header_used += take;
            bytes = bytes.get(take..).ok_or(Error::Protocol)?;
            if self.header_used < 4 {
                return Ok(());
            }
            let size =
                usize::try_from(u32::from_be_bytes(self.header)).map_err(|_| Error::Limit)?;
            if size == 0 {
                return Err(Error::Protocol);
            }
            if size > MAX_FRAME {
                return Err(Error::Limit);
            }
            self.payload = vec![0; size];
        }
        let end = self.used.checked_add(bytes.len()).ok_or(Error::Limit)?;
        self.payload
            .get_mut(self.used..end)
            .ok_or(Error::Protocol)?
            .copy_from_slice(bytes);
        self.used = end;
        Ok(())
    }

    pub fn payload(&self) -> Option<&[u8]> {
        (!self.failed && self.header_used == 4 && self.used == self.payload.len())
            .then_some(self.payload.as_slice())
    }

    /// EOF before a complete payload is an error, not a shorter request.
    pub fn finish(self) -> Result<Vec<u8>> {
        if self.payload().is_none() {
            return Err(Error::Protocol);
        }
        Ok(self.payload)
    }
}

pub fn frame(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.is_empty() {
        return Err(Error::Protocol);
    }
    if payload.len() > MAX_FRAME {
        return Err(Error::Limit);
    }
    let length = u32::try_from(payload.len()).map_err(|_| Error::Limit)?;
    let mut framed = Vec::with_capacity(payload.len() + 4);
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(payload);
    Ok(framed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Query {
    State,
    Text {
        tab: TabId,
        revision: u64,
        offset: usize,
        limit: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Request {
    pub id: u64,
    pub query: Query,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub id: u64,
    pub error: Error,
}

impl Refusal {
    pub fn response(self) -> String {
        format!(
            "1\t{}\terror\t{}\t{}",
            self.id,
            self.error.code(),
            hex(self.error.code().as_bytes())
        )
    }
}

impl Request {
    /// Parse the implemented read-only subset. Recoverable IDs echo on errors;
    /// errors before a recoverable ID use zero, matching replay.
    pub fn parse(input: &[u8]) -> std::result::Result<Self, Refusal> {
        let Envelope { id, name, mut args } = envelope(input)?;
        let result = (|| {
            let query = match name {
                "state" => Query::State,
                "text" => Query::Text {
                    tab: decimal(args.next().ok_or(Error::Protocol)?)?,
                    revision: decimal(args.next().ok_or(Error::Protocol)?)?,
                    offset: size(args.next().ok_or(Error::Protocol)?)?,
                    limit: size(args.next().ok_or(Error::Protocol)?)?,
                },
                _ => return Err(Error::Protocol),
            };
            if args.next().is_some() {
                return Err(Error::Protocol);
            }
            Ok(query)
        })();
        result
            .map(|query| Self { id, query })
            .map_err(|error| Refusal { id, error })
    }

    /// State is a controller snapshot only. A future window endpoint must add
    /// its dialogs, jobs and submitted/callback-completed frame generations.
    pub fn response(self, ui: &Controller) -> String {
        let result = match self.query {
            Query::State => state(ui),
            Query::Text {
                tab,
                revision,
                offset,
                limit,
            } => page(ui, tab, revision, offset, limit),
        };
        match result {
            Ok(body) => format!("1\t{}\tok\t{body}", self.id),
            Err(error) => Refusal { id: self.id, error }.response(),
        }
    }
}

pub(crate) struct Envelope<'a> {
    pub id: u64,
    pub name: &'a str,
    pub args: std::str::Split<'a, char>,
}

pub(crate) fn envelope(input: &[u8]) -> std::result::Result<Envelope<'_>, Refusal> {
    let mut id = 0;
    let result = (|| {
        if input.len() > MAX_FRAME {
            return Err(Error::Limit);
        }
        let input = std::str::from_utf8(input).map_err(|_| Error::Protocol)?;
        if !input.is_ascii() || input.bytes().any(|b| b < b' ' && b != b'\t' || b == 127) {
            return Err(Error::Protocol);
        }
        let mut args = input.split('\t');
        if args.next() != Some("1") {
            return Err(Error::Protocol);
        }
        id = decimal(args.next().ok_or(Error::Protocol)?)?;
        let name = args.next().ok_or(Error::Protocol)?;
        Ok((name, args))
    })();
    result
        .map(|(name, args)| Envelope { id, name, args })
        .map_err(|error| Refusal { id, error })
}

pub(crate) fn decimal(text: &str) -> Result<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Protocol);
    }
    text.parse().map_err(|_| Error::Protocol)
}

pub(crate) fn size(text: &str) -> Result<usize> {
    usize::try_from(decimal(text)?).map_err(|_| Error::Protocol)
}

pub fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".into();
    }
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        for index in [usize::from(byte >> 4), usize::from(byte & 15)] {
            if let Some(&digit) = DIGITS.get(index) {
                out.push(char::from(digit));
            }
        }
    }
    out
}

pub fn unhex(value: &str) -> Result<Vec<u8>> {
    if value == "-" {
        return Ok(Vec::new());
    }
    if value.is_empty() || value.len() > MAX_FRAME || !value.len().is_multiple_of(2) {
        return Err(Error::Protocol);
    }
    let digit = |b: u8| -> Result<u8> {
        match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(Error::Protocol),
        }
    };
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = digit(*pair.first().ok_or(Error::Protocol)?)?;
            let low = digit(*pair.get(1).ok_or(Error::Protocol)?)?;
            Ok(high * 16 + low)
        })
        .collect()
}

pub(crate) fn state(ui: &Controller) -> Result<String> {
    let mut out = format!(
        "active={}\tkeys={}\tprefix={}",
        ui.editor().active().unwrap_or(0),
        match ui.keys().profile() {
            crate::keys::Profile::Windows => "windows",
            crate::keys::Profile::Emacs => "emacs",
        },
        u8::from(ui.keys().pending())
    );
    for (id, doc) in ui.editor().tabs() {
        let sel = doc.selection();
        out.push_str(&format!(
            "\ttab={id},{},{},{},{},{},{},{},{},{}",
            doc.revision(),
            u8::from(doc.dirty()),
            doc.text().len(),
            sel.anchor,
            sel.caret,
            u8::from(doc.auto_fill()),
            doc.fill_column(),
            u8::from(doc.format().bom),
            match doc.format().ending {
                crate::text::LineEnding::Lf => "lf",
                crate::text::LineEnding::CrLf => "crlf",
            }
        ));
    }
    let (width, height) = ui.geometry().dimensions();
    out.push_str(&format!(
        "\tgeneration={}\twindow={width},{height},{}\tfocus={}",
        ui.generation(),
        ui.geometry().scale().value(),
        u8::from(ui.focused())
    ));
    for (id, _) in ui.editor().tabs() {
        let view = ui.tab_view(id)?;
        let origin = view.viewport.origin();
        let (columns, rows) = view.viewport.dimensions();
        out.push_str(&format!(
            "\tview={id},{},{},{columns},{rows},{},{},{}",
            origin.row,
            origin.column,
            u8::from(view.soft_wrap),
            match view.affinity {
                crate::layout::Affinity::Upstream => "upstream",
                crate::layout::Affinity::Downstream => "downstream",
            },
            view.desired_column
                .map_or_else(|| "-".into(), |value| value.to_string())
        ));
    }
    Ok(out)
}

pub(crate) fn page(
    ui: &Controller,
    tab: TabId,
    revision: u64,
    offset: usize,
    limit: usize,
) -> Result<String> {
    let doc = ui.editor().document(tab)?;
    if doc.revision() != revision {
        return Err(Error::StaleRevision);
    }
    if !(4..=PAGE_BYTES).contains(&limit) {
        return Err(Error::InvalidArgument);
    }
    doc.text().get(offset..).ok_or(Error::InvalidPosition)?;
    let mut end = offset.saturating_add(limit).min(doc.text().len());
    while !doc.text().is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    Ok(format!(
        "{end}\t{}",
        hex(doc
            .text()
            .get(offset..end)
            .ok_or(Error::InvalidPosition)?
            .as_bytes())
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{Command, Selection};
    use crate::ui::Event;

    #[test]
    fn every_frame_split_and_single_byte_delivery_wait_for_complete_payload() {
        let payload = b"1\t17\ttext\t1\t0\t0\t4";
        let bytes = frame(payload).unwrap();
        for split in 0..=bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..split).unwrap()).unwrap();
            assert_eq!(
                decoder.payload(),
                (split == bytes.len()).then_some(payload.as_slice())
            );
            decoder.push(bytes.get(split..).unwrap()).unwrap();
            assert_eq!(decoder.payload(), Some(payload.as_slice()));
            assert_eq!(decoder.finish().unwrap(), payload);
        }
        let mut decoder = Decoder::default();
        for (index, byte) in bytes.iter().enumerate() {
            decoder.push(std::slice::from_ref(byte)).unwrap();
            assert_eq!(decoder.payload().is_some(), index + 1 == bytes.len());
        }
        assert_eq!(
            Request::parse(&decoder.finish().unwrap()).unwrap(),
            Request {
                id: 17,
                query: Query::Text {
                    tab: 1,
                    revision: 0,
                    offset: 0,
                    limit: 4
                },
            }
        );
    }

    #[test]
    fn frame_limits_truncation_and_trailing_bytes_never_publish_partial_requests() {
        let bytes = frame(b"1\t0\tstate").unwrap();
        for end in 0..bytes.len() {
            let mut decoder = Decoder::default();
            decoder.push(bytes.get(..end).unwrap()).unwrap();
            assert_eq!(decoder.finish(), Err(Error::Protocol));
        }
        for length in [0u32, MAX_FRAME as u32 + 1, u32::MAX] {
            let mut decoder = Decoder::default();
            assert!(decoder.push(&length.to_be_bytes()).is_err());
            assert!(decoder.payload.is_empty());
            assert_eq!(decoder.push(&bytes), Err(Error::Protocol));
            assert!(decoder.payload().is_none());
        }
        let mut decoder = Decoder::default();
        decoder.push(&bytes).unwrap();
        assert_eq!(decoder.push(b"x"), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        let mut joined = bytes.clone();
        joined.extend_from_slice(&bytes);
        let mut decoder = Decoder::default();
        assert_eq!(decoder.push(&joined), Err(Error::Protocol));
        assert!(decoder.payload().is_none());
        assert_eq!(frame(b""), Err(Error::Protocol));
        assert_eq!(frame(&vec![b'x'; MAX_FRAME + 1]), Err(Error::Limit));
        let limit = frame(&vec![b'x'; MAX_FRAME]).unwrap();
        let mut decoder = Decoder::default();
        decoder.push(&limit).unwrap();
        assert_eq!(decoder.finish().unwrap().len(), MAX_FRAME);
    }

    #[test]
    fn strict_read_only_request_grammar_rejects_mutation_and_recovers_only_valid_ids() {
        for (input, id) in [
            ("", 0),
            ("2\t12\tstate", 0),
            ("1\t+1\tstate", 0),
            ("1\t18446744073709551616\tstate", 0),
            ("1\t12\tstate\n", 0),
            ("1\t12\tstáte", 0),
            ("1\t12\tstate\t", 12),
            ("1\t12\tnew", 12),
            ("1\t12\tinsert\t1\t0\t61", 12),
            ("1\t12\ttext\t1\t0\t0", 12),
            ("1\t12\ttext\t1\t0\t-1\t4", 12),
            ("1\t12\ttext\t1\t0\t0\t4\textra", 12),
        ] {
            assert_eq!(
                Request::parse(input.as_bytes()),
                Err(Refusal {
                    id,
                    error: Error::Protocol
                }),
                "{input}"
            );
        }
        assert_eq!(Request::parse(b"1\t000\tstate").unwrap().id, 0);
        assert_eq!(
            Request::parse(b"1\t18446744073709551615\tstate")
                .unwrap()
                .id,
            u64::MAX
        );
        assert_eq!(
            Request::parse(&vec![b'\t'; MAX_FRAME + 1]),
            Err(Refusal {
                id: 0,
                error: Error::Limit
            })
        );
        assert!(Request::parse(&vec![b'\t'; MAX_FRAME]).is_err());
        assert_eq!(
            Request::parse(&[0xff]),
            Err(Refusal {
                id: 0,
                error: Error::Protocol
            })
        );
        assert_eq!(
            Refusal {
                id: 9,
                error: Error::StaleRevision
            }
            .response(),
            "1\t9\terror\tstale-revision\t7374616c652d7265766973696f6e"
        );
    }

    #[test]
    fn read_only_snapshots_and_scalar_pages_match_replay_without_mutating_state() {
        let mut replay = crate::replay::Session::default();
        replay.ui.dispatch(Event::Load("aλ🦀z".as_bytes())).unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::Select(Selection {
                    anchor: 7,
                    caret: 1,
                }),
            })
            .unwrap();
        for input in [
            "1\t3\tstate",
            "1\t3\ttext\t1\t0\t0\t4",
            "1\t3\ttext\t1\t0\t3\t4",
            "1\t3\ttext\t1\t0\t7\t4",
            "1\t3\ttext\t1\t0\t8\t4",
            "1\t3\ttext\t1\t0\t2\t4",
            "1\t3\ttext\t1\t1\t0\t4",
            "1\t3\ttext\t1\t0\t0\t3",
            "1\t3\ttext\t2\t0\t0\t4",
        ] {
            let before = format!("{:?}", replay.ui.editor());
            let generation = replay.ui.generation();
            let view = replay.ui.tab_view(1).unwrap();
            let response = Request::parse(input.as_bytes())
                .unwrap()
                .response(&replay.ui);
            assert_eq!(response, replay.request(input.as_bytes()));
            assert_eq!(format!("{:?}", replay.ui.editor()), before);
            assert_eq!(replay.ui.generation(), generation);
            assert_eq!(replay.ui.tab_view(1).unwrap(), view);
            assert!(response.len() <= MAX_FRAME);
            assert!(frame(response.as_bytes()).is_ok());
        }
        assert_eq!(page(&replay.ui, 1, 0, 0, 4).unwrap(), "3\t61cebb");
        assert_eq!(page(&replay.ui, 1, 0, 3, 4).unwrap(), "7\tf09fa680");
        assert_eq!(page(&replay.ui, 1, 0, 8, 4).unwrap(), "8\t-");
        assert_eq!(
            page(&replay.ui, 1, 0, usize::MAX, 4),
            Err(Error::InvalidPosition)
        );
        assert_eq!(
            page(&replay.ui, 1, 0, 0, PAGE_BYTES + 1),
            Err(Error::InvalidArgument)
        );
        let request = Request::parse(b"1\t3\ttext\t1\t0\t0\t4").unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 0,
                command: Command::Insert("b".into()),
            })
            .unwrap();
        replay
            .ui
            .dispatch(Event::Edit {
                tab: 1,
                revision: 1,
                command: Command::Undo,
            })
            .unwrap();
        assert!(request
            .response(&replay.ui)
            .contains("error\tstale-revision"));
    }

    #[test]
    fn maximum_page_and_tab_snapshot_fit_the_frame_ceiling() {
        let mut ui = Controller::default();
        ui.dispatch(Event::Load(&vec![b'x'; PAGE_BYTES + 1]))
            .unwrap();
        let response = Request {
            id: u64::MAX,
            query: Query::Text {
                tab: 1,
                revision: 0,
                offset: 0,
                limit: PAGE_BYTES,
            },
        }
        .response(&ui);
        assert!(response.ends_with(&"78".repeat(PAGE_BYTES)));
        assert!(frame(response.as_bytes()).is_ok());
        for _ in 1..64 {
            ui.dispatch(Event::New).unwrap();
        }
        let response = Request {
            id: 1,
            query: Query::State,
        }
        .response(&ui);
        assert_eq!(response.matches("\ttab=").count(), 64);
        assert_eq!(response.matches("\tview=").count(), 64);
        assert!(frame(response.as_bytes()).is_ok());
    }

    #[test]
    fn binary_text_encoding_and_arbitrary_framed_input_have_closed_error_paths() {
        let bytes: Vec<_> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(b""), "-");
        for invalid in ["", "A0", "g0", "0", "--"] {
            assert!(unhex(invalid).is_err());
        }
        let mut completed = 0;
        for seed in 0u64..1000 {
            let mut value = seed;
            let bytes: Vec<_> = (0..64)
                .map(|_| {
                    value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (value >> 32) as u8
                })
                .collect();
            let mut decoder = Decoder::default();
            let _ = decoder.push(&bytes);
            if let Some(payload) = decoder.payload() {
                let _ = Request::parse(payload);
            }
            let _ = decoder.finish();
            let _ = Request::parse(&bytes);
            if seed % 3 == 0 {
                let framed = frame(&bytes).unwrap();
                let mut decoder = Decoder::default();
                for part in framed.chunks((seed % 7 + 1) as usize) {
                    decoder.push(part).unwrap();
                }
                assert_eq!(decoder.payload(), Some(bytes.as_slice()));
                let _ = Request::parse(decoder.payload().unwrap());
                assert_eq!(decoder.finish().unwrap(), bytes);
                completed += 1;
            }
        }
        assert_eq!(completed, 334);
    }

    #[test]
    fn malformed_envelopes_and_read_only_commands_echo_identical_refusals_in_replay() {
        let mut replay = crate::replay::Session::default();
        for input in [
            b"".as_slice(),
            b"2\t12\tstate",
            b"1\t+1\tstate",
            b"1\t18446744073709551616\tstate",
            b"1\t12\tstate\n",
            "1\t12\tstáte".as_bytes(),
            b"1\t12",
            b"1\t12\tstate\t",
            b"1\t12\ttext\t1\t0\t0",
            b"1\t12\ttext\t1\t0\t-1\t4",
            b"1\t12\ttext\t1\t0\t0\t4\textra",
            &[0xff],
            &vec![b'\t'; MAX_FRAME + 1],
        ] {
            let refusal = Request::parse(input).unwrap_err();
            assert_eq!(refusal.response(), replay.request(input));
            assert_eq!(replay.ui.generation(), 0);
            assert_eq!(replay.ui.editor().tabs().count(), 0);
        }
    }
}
