//! Server-sent events as a model provider streams a reply (DESIGN.md §5):
//! the `text/event-stream` format of the WHATWG HTML standard, read from
//! bytes as the fetch service hands them over, a frame at a time, with an
//! event free to straddle any number of frames.
//!
//! Lines end in LF, CRLF or a lone CR, a CRLF split across two frames
//! included. A line that begins with `:` is a comment and is skipped:
//! OpenRouter sends `: OPENROUTER PROCESSING` while a model has nothing to
//! say yet. A `data` field's value is the event's text, several `data`
//! lines joined by a line feed; the other fields (`event`, `id`, `retry`)
//! mean nothing to a chat completion and are skipped. A blank line
//! dispatches the event. `data: [DONE]` ends the stream, and nothing after
//! it is read. An event cut off by the stream's end is not dispatched, as
//! the standard says.
//!
//! Each line and each event's text are held to `max_event` bytes, and the
//! whole stream to `max_total`, so nothing a provider sends grows a buffer
//! past a bound; the buffers are kept and reused from event to event.

/// The longest line, and the longest event's text, a model stream may send.
pub const MAX_EVENT: usize = 256 * 1024;

/// One dispatched event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event<'a> {
    /// An event's `data`, its lines joined by line feeds.
    Data(&'a str),
    /// `data: [DONE]`: the stream is whole.
    Done,
}

/// Why the reader stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Error {
    /// A line or an event's text past `max_event` bytes.
    Event(usize),
    /// The stream past `max_total` bytes.
    Total(u64),
    /// An event's text that is not UTF-8.
    Utf8,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Event(bound) => write!(f, "a stream event past {bound} bytes"),
            Self::Total(bound) => write!(f, "a stream past {bound} bytes"),
            Self::Utf8 => write!(f, "a stream event that is not UTF-8"),
        }
    }
}

/// What ended a `feed`: the reader's own refusal, or the sink's.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Fault<E> {
    Reader(Error),
    Sink(E),
}

/// The byte order mark a stream may begin with, which is not its text.
const BOM: &[u8] = b"\xEF\xBB\xBF";

/// An event stream being read.
#[derive(Debug)]
pub struct Reader {
    /// The line under way, without its ending.
    line: Vec<u8>,
    /// The event under way's text, each `data` line followed by a line
    /// feed until it is dispatched.
    data: Vec<u8>,
    /// Whether a `data` line has come since the last dispatch: an event
    /// of one empty `data` line is dispatched, with empty text.
    has_data: bool,
    /// The last byte ended a line with a CR, so an LF next is its CRLF.
    after_cr: bool,
    /// The first line is done, its byte order mark stripped.
    started: bool,
    total: u64,
    max_event: usize,
    max_total: u64,
    done: bool,
}

impl Reader {
    pub fn new(max_event: usize, max_total: u64) -> Self {
        Self {
            line: Vec::new(),
            data: Vec::new(),
            has_data: false,
            after_cr: false,
            started: false,
            total: 0,
            max_event,
            max_total,
            done: false,
        }
    }

    /// Whether `data: [DONE]` has come.
    pub fn done(&self) -> bool {
        self.done
    }

    /// Reads `bytes`, handing `sink` each event they complete, in order;
    /// a sink's error stops the reading and is handed back. Bytes after
    /// `[DONE]` are not read.
    pub fn feed<E>(
        &mut self,
        bytes: &[u8],
        sink: &mut dyn FnMut(Event<'_>) -> Result<(), E>,
    ) -> Result<(), Fault<E>> {
        if self.done {
            return Ok(());
        }
        self.total = self.total.saturating_add(bytes.len() as u64);
        if self.total > self.max_total {
            return Err(Fault::Reader(Error::Total(self.max_total)));
        }
        for &byte in bytes {
            match byte {
                // The LF of a CRLF whose CR ended the line already.
                b'\n' if self.after_cr => self.after_cr = false,
                b'\n' | b'\r' => {
                    self.after_cr = byte == b'\r';
                    self.end_line(sink)?;
                    if self.done {
                        return Ok(());
                    }
                }
                _ => {
                    self.after_cr = false;
                    if self.line.len() >= self.max_event {
                        return Err(Fault::Reader(Error::Event(self.max_event)));
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(())
    }

    fn end_line<E>(
        &mut self,
        sink: &mut dyn FnMut(Event<'_>) -> Result<(), E>,
    ) -> Result<(), Fault<E>> {
        let mut line = std::mem::take(&mut self.line);
        let result = self.line_of(&mut line, sink);
        line.clear();
        self.line = line;
        result
    }

    fn line_of<E>(
        &mut self,
        line: &mut Vec<u8>,
        sink: &mut dyn FnMut(Event<'_>) -> Result<(), E>,
    ) -> Result<(), Fault<E>> {
        if !self.started {
            self.started = true;
            if line.starts_with(BOM) {
                line.drain(..BOM.len());
            }
        }
        if line.is_empty() {
            return self.dispatch(sink);
        }
        if line.first() == Some(&b':') {
            return Ok(());
        }
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let field = line.get(..colon).unwrap_or_default();
                let value = line.get(colon + 1..).unwrap_or_default();
                (field, value.strip_prefix(b" ").unwrap_or(value))
            }
            None => (line.as_slice(), &[][..]),
        };
        if field == b"data" {
            // The event's text: what it holds, joined, and this line.
            if self.data.len().saturating_add(value.len()) > self.max_event {
                return Err(Fault::Reader(Error::Event(self.max_event)));
            }
            self.data.extend_from_slice(value);
            self.data.push(b'\n');
            self.has_data = true;
        }
        Ok(())
    }

    fn dispatch<E>(
        &mut self,
        sink: &mut dyn FnMut(Event<'_>) -> Result<(), E>,
    ) -> Result<(), Fault<E>> {
        if !self.has_data {
            return Ok(());
        }
        self.has_data = false;
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        let result = match std::str::from_utf8(&data) {
            Err(_) => Err(Fault::Reader(Error::Utf8)),
            Ok("[DONE]") => {
                self.done = true;
                sink(Event::Done).map_err(Fault::Sink)
            }
            Ok(text) => sink(Event::Data(text)).map_err(Fault::Sink),
        };
        data.clear();
        self.data = data;
        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;

    /// What `stream` dispatches when fed in the pieces `cuts` makes, each
    /// event as its text, `[DONE]` as itself.
    fn read(stream: &[u8], cuts: &[usize]) -> Result<Vec<String>, Fault<()>> {
        let mut reader = Reader::new(MAX_EVENT, 1 << 20);
        let mut events = Vec::new();
        let mut at = 0;
        for &cut in cuts.iter().chain(std::iter::once(&stream.len())) {
            reader.feed(&stream[at..cut], &mut |event| {
                events.push(match event {
                    Event::Data(text) => text.to_string(),
                    Event::Done => "[DONE]".to_string(),
                });
                Ok(())
            })?;
            at = cut;
        }
        Ok(events)
    }

    const OPENROUTER: &str = ": OPENROUTER PROCESSING\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
        : OPENROUTER PROCESSING\n\n\
        data: {\"choices\":[{\"delta\":{\"content\":\"lo \\u00e9\"}}]}\n\n\
        data: [DONE]\n\n";

    #[test]
    fn data_lines_are_events_and_comment_lines_are_skipped() {
        assert_eq!(
            read(OPENROUTER.as_bytes(), &[]).unwrap(),
            [
                "{\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}",
                "{\"choices\":[{\"delta\":{\"content\":\"lo \\u00e9\"}}]}",
                "[DONE]"
            ]
        );
    }

    /// Every byte boundary of a stream, CRLF and a multibyte character
    /// among them, splits it the same.
    #[test]
    fn an_event_split_at_any_byte_reads_the_same() {
        let crlf = OPENROUTER.replace('\n', "\r\n");
        let multibyte = "data: caf\u{e9} \u{1f600}\n\ndata: [DONE]\n\n";
        for stream in [OPENROUTER, crlf.as_str(), multibyte] {
            let whole = read(stream.as_bytes(), &[]).unwrap();
            for cut in 0..=stream.len() {
                assert_eq!(read(stream.as_bytes(), &[cut]).unwrap(), whole, "{cut}");
            }
            // And a byte at a time.
            let every: Vec<usize> = (1..stream.len()).collect();
            assert_eq!(read(stream.as_bytes(), &every).unwrap(), whole);
        }
    }

    #[test]
    fn lf_crlf_and_cr_endings_read_alike() {
        let lf = "data: a\n\ndata: b\n\n";
        let crlf = "data: a\r\n\r\ndata: b\r\n\r\n";
        let cr = "data: a\r\rdata: b\r\r";
        let mixed = "data: a\r\n\ndata: b\r\r\n";
        for stream in [lf, crlf, cr, mixed] {
            assert_eq!(
                read(stream.as_bytes(), &[]).unwrap(),
                ["a", "b"],
                "{stream:?}"
            );
        }
    }

    #[test]
    fn several_data_lines_are_one_event_joined_by_line_feeds() {
        let stream = "data: {\"a\":\ndata:1}\nevent: message\nid: 7\nretry: 10\n\n";
        assert_eq!(read(stream.as_bytes(), &[]).unwrap(), ["{\"a\":\n1}"]);
        // One empty data line is an event of empty text; a blank line with
        // no data is none, nor are fields other than data.
        assert_eq!(read(b"data\n\n\n\nevent: x\n\n", &[]).unwrap(), [""]);
        // A leading byte order mark is not the first line's.
        assert_eq!(read(b"\xEF\xBB\xBFdata: x\n\n", &[]).unwrap(), ["x"]);
    }

    #[test]
    fn done_ends_the_stream_and_what_follows_is_not_read() {
        let mut reader = Reader::new(MAX_EVENT, 1 << 20);
        let mut events = 0;
        let mut sink = |_: Event<'_>| -> Result<(), ()> {
            events += 1;
            Ok(())
        };
        reader
            .feed(b"data: [DONE]\n\ndata: late\n\n", &mut sink)
            .unwrap();
        assert!(reader.done());
        reader.feed(b"data: later\n\n", &mut sink).unwrap();
        assert_eq!(events, 1);
        // An event the stream's end cut off is never dispatched.
        assert_eq!(read(b"data: half", &[]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn each_event_and_the_total_are_bounded() {
        let mut reader = Reader::new(16, 64);
        let mut sink = |_: Event<'_>| -> Result<(), ()> { Ok(()) };
        // A line past the bound, before its end comes.
        assert_eq!(
            reader.feed(&[b'x'; 17], &mut sink),
            Err(Fault::Reader(Error::Event(16)))
        );
        // An event's text past it across several data lines.
        let mut reader = Reader::new(16, 64);
        assert_eq!(
            reader.feed(b"data: 12345678\ndata: 12345678\n", &mut sink),
            Err(Fault::Reader(Error::Event(16)))
        );
        // An event's text of exactly the bound is read.
        let mut reader = Reader::new(16, 64);
        let mut seen = Vec::new();
        reader
            .feed(b"data: 1234567\ndata: 12345678\n\n", &mut |event| {
                if let Event::Data(text) = event {
                    seen.push(text.len());
                }
                Ok::<(), ()>(())
            })
            .unwrap();
        assert_eq!(seen, [16]);
        // A comment line is held to it too.
        let mut reader = Reader::new(16, 64);
        assert!(reader.feed(b": 0123456789abcdef\n", &mut sink).is_err());
        // The whole stream, however its events are cut.
        let mut reader = Reader::new(16, 64);
        for _ in 0..7 {
            reader.feed(b"data: 1\n\n", &mut sink).unwrap();
        }
        assert_eq!(
            reader.feed(b"data: 1\n\n", &mut sink),
            Err(Fault::Reader(Error::Total(64)))
        );
        assert_eq!(
            read(b"data: \xFF\n\n", &[]),
            Err(Fault::Reader(Error::Utf8))
        );
    }

    #[test]
    fn a_sink_error_stops_the_reading() {
        let mut reader = Reader::new(MAX_EVENT, 1 << 20);
        let mut seen = Vec::new();
        let result = reader.feed(b"data: 1\n\ndata: 2\n\ndata: 3\n\n", &mut |event| {
            if let Event::Data(text) = event {
                seen.push(text.to_string());
            }
            if seen.len() == 2 {
                Err("stop")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err(Fault::Sink("stop")));
        assert_eq!(seen, ["1", "2"]);
    }
}
