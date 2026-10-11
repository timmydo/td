//! Inlines: code spans, emphasis, strikethrough, links, images, autolinks
//! and line breaks, over a paragraph's lines joined by newlines.

use std::collections::HashMap;
use std::ops::Range;

use crate::{Link, Span, SpanKind, Style};

/// The deepest nesting of parentheses in a link's destination.
const MAX_PARENS: usize = 32;
/// The longest autolink scheme.
const MAX_SCHEME: usize = 32;

/// A delimiter run of `*`, `_` or `~`: what of it is left as text
/// (`lo..hi`, as emphasis takes its characters from the inside), its
/// length as written, and whether it can open or close.
struct Delim {
    ch: u8,
    lo: usize,
    hi: usize,
    orig: usize,
    open: bool,
    close: bool,
}

impl Delim {
    fn count(&self) -> usize {
        self.hi - self.lo
    }
}

enum Item {
    Text(Range<usize>),
    Code(Range<usize>),
    Delim(Delim),
    /// `[` or `![` not yet closed, and how many links were named before
    /// it: an image drops those named inside it.
    Bracket {
        start: usize,
        after: usize,
        image: bool,
        links: usize,
    },
    LinkStart(usize),
    LinkEnd,
    Image {
        alt: Range<usize>,
        source: usize,
        markup: Range<usize>,
    },
    /// A line ending, at its newline.
    Soft(usize),
    Hard(usize),
    /// An autolink's mark inside a link, which gives way to it.
    Gone,
}

/// A delimiter in the doubly linked list emphasis is matched over.
struct Node {
    item: usize,
    prev: Option<usize>,
    next: Option<usize>,
}

struct Inline<'a> {
    /// The lines, joined by newlines.
    text: String,
    /// Where each line starts in `text` and in the source.
    segments: Vec<(usize, usize)>,
    items: Vec<Item>,
    nodes: Vec<Node>,
    tail: Option<usize>,
    /// Each open bracket's item and the delimiters before it.
    brackets: Vec<(usize, usize)>,
    /// The `[` brackets below this depth are inactive: a link closed after
    /// them, and links do not nest. One mark, not a walk over the stack,
    /// so a run of brackets and links stays linear.
    inactive_below: usize,
    /// Emphasis matched: its opener's item, its closer's, and its style.
    pairs: Vec<(usize, usize, Style)>,
    links: &'a mut Vec<Link>,
    /// Each backtick run's start, by its length.
    ticks: HashMap<usize, Vec<usize>>,
}

fn punctuation(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_ascii() && !c.is_alphanumeric() && !c.is_whitespace())
}

fn space(c: Option<char>) -> bool {
    c.is_none_or(char::is_whitespace)
}

/// Parses the inlines of `lines`, recording the links they name in
/// `links`.
pub(crate) fn parse(source: &str, lines: &[Range<usize>], links: &mut Vec<Link>) -> Vec<Span> {
    let mut text = String::new();
    let mut segments = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        segments.push((text.len(), line.start));
        text.push_str(source.get(line.clone()).unwrap_or(""));
    }
    let mut ticks: HashMap<usize, Vec<usize>> = HashMap::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes.get(at) == Some(&b'`') {
            let length = bytes
                .get(at..)
                .unwrap_or(&[])
                .iter()
                .take_while(|&&b| b == b'`')
                .count();
            ticks.entry(length).or_default().push(at);
            at += length;
        } else {
            at += 1;
        }
    }
    let mut inline = Inline {
        text,
        segments,
        items: Vec::new(),
        nodes: Vec::new(),
        tail: None,
        brackets: Vec::new(),
        inactive_below: 0,
        pairs: Vec::new(),
        links,
        ticks,
    };
    inline.scan();
    inline.process_emphasis(0);
    inline.spans()
}

impl Inline<'_> {
    fn byte(&self, at: usize) -> Option<u8> {
        self.text.as_bytes().get(at).copied()
    }

    fn before(&self, at: usize) -> Option<char> {
        self.text
            .get(..at)
            .and_then(|head| head.chars().next_back())
    }

    fn after(&self, at: usize) -> Option<char> {
        self.text.get(at..).and_then(|tail| tail.chars().next())
    }

    /// `at` in the text as a source offset: on a line, or at a line's end.
    fn map(&self, at: usize) -> usize {
        let index = self
            .segments
            .partition_point(|&(start, _)| start <= at)
            .saturating_sub(1);
        self.segments
            .get(index)
            .map_or(0, |&(start, source)| source + (at - start))
    }

    fn map_range(&self, range: &Range<usize>) -> Range<usize> {
        let start = self.map(range.start);
        start..self.map(range.end).max(start)
    }

    fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    fn text_item(&mut self, from: usize, to: usize) {
        if from < to {
            self.push(Item::Text(from..to));
        }
    }

    fn scan(&mut self) {
        let len = self.text.len();
        let mut at = 0;
        let mut text = 0;
        while at < len {
            let Some(b) = self.byte(at) else {
                break;
            };
            match b {
                b'\\' => match self.byte(at + 1) {
                    Some(b'\n') => {
                        self.text_item(text, at);
                        self.push(Item::Hard(at + 1));
                        at += 2;
                        text = at;
                    }
                    Some(next) if next.is_ascii_punctuation() => {
                        self.text_item(text, at);
                        self.push(Item::Text(at + 1..at + 2));
                        at += 2;
                        text = at;
                    }
                    _ => at += 1,
                },
                b'`' => {
                    let length = self
                        .text
                        .as_bytes()
                        .get(at..)
                        .unwrap_or(&[])
                        .iter()
                        .take_while(|&&b| b == b'`')
                        .count();
                    let close = self.ticks.get(&length).and_then(|starts| {
                        let next = starts.partition_point(|&start| start <= at);
                        starts.get(next).copied()
                    });
                    match close {
                        Some(close) => {
                            self.text_item(text, at);
                            let mut content = at + length..close;
                            let inner = self.text.get(content.clone()).unwrap_or("");
                            let padded = |c: Option<char>| c == Some(' ') || c == Some('\n');
                            if inner.len() >= 2
                                && padded(inner.chars().next())
                                && padded(inner.chars().next_back())
                                && !inner.bytes().all(|b| b == b' ' || b == b'\n')
                            {
                                content = content.start + 1..content.end - 1;
                            }
                            self.push(Item::Code(content));
                            at = close + length;
                            text = at;
                        }
                        None => at += length,
                    }
                }
                b'*' | b'_' | b'~' => {
                    let length = self
                        .text
                        .as_bytes()
                        .get(at..)
                        .unwrap_or(&[])
                        .iter()
                        .take_while(|&&c| c == b)
                        .count();
                    self.text_item(text, at);
                    if b == b'~' && length > 2 {
                        self.push(Item::Text(at..at + length));
                    } else {
                        self.delimiter(b, at, length);
                    }
                    at += length;
                    text = at;
                }
                b'[' => {
                    self.text_item(text, at);
                    self.bracket(at, at + 1, false);
                    at += 1;
                    text = at;
                }
                b'!' if self.byte(at + 1) == Some(b'[') => {
                    self.text_item(text, at);
                    self.bracket(at, at + 2, true);
                    at += 2;
                    text = at;
                }
                b']' => {
                    self.text_item(text, at);
                    at = self.close_bracket(at);
                    text = at;
                }
                b'<' => match self.autolink(at) {
                    Some(end) => {
                        self.text_item(text, at);
                        self.link_text(at + 1..end, at + 1..end);
                        at = end + 1;
                        text = at;
                    }
                    None => at += 1,
                },
                b'\n' => {
                    // Two spaces before it make a hard break; tabs do not.
                    let spaces = self
                        .text
                        .as_bytes()
                        .get(text..at)
                        .unwrap_or(&[])
                        .iter()
                        .rev()
                        .take_while(|&&b| b == b' ')
                        .count();
                    let mut trail = at;
                    while trail > text && matches!(self.byte(trail - 1), Some(b' ' | b'\t')) {
                        trail -= 1;
                    }
                    self.text_item(text, trail);
                    self.push(if spaces >= 2 {
                        Item::Hard(at)
                    } else {
                        Item::Soft(at)
                    });
                    at += 1;
                    text = at;
                }
                b'h' | b'H' => match self.bare_url(at) {
                    Some(end) => {
                        self.text_item(text, at);
                        self.link_text(at..end, at..end);
                        at = end;
                        text = at;
                    }
                    None => at += 1,
                },
                _ => at += 1,
            }
        }
        // The paragraph's trailing whitespace shows nothing.
        let mut end = len;
        while end > text && matches!(self.byte(end - 1), Some(b' ' | b'\t')) {
            end -= 1;
        }
        self.text_item(text, end);
    }

    /// A delimiter run at `at`, which can open or close as its flanking
    /// says.
    fn delimiter(&mut self, ch: u8, at: usize, length: usize) {
        let before = self.before(at);
        let after = self.after(at + length);
        let left = !space(after)
            && (!after.is_some_and(punctuation)
                || space(before)
                || before.is_some_and(punctuation));
        let right = !space(before)
            && (!before.is_some_and(punctuation) || space(after) || after.is_some_and(punctuation));
        let (open, close) = if ch == b'_' {
            (
                left && (!right || before.is_some_and(punctuation)),
                right && (!left || after.is_some_and(punctuation)),
            )
        } else {
            (left, right)
        };
        let item = self.items.len();
        self.push(Item::Delim(Delim {
            ch,
            lo: at,
            hi: at + length,
            orig: length,
            open,
            close,
        }));
        let node = self.nodes.len();
        self.nodes.push(Node {
            item,
            prev: self.tail,
            next: None,
        });
        if let Some(tail) = self.tail.and_then(|tail| self.nodes.get_mut(tail)) {
            tail.next = Some(node);
        }
        self.tail = Some(node);
    }

    fn bracket(&mut self, start: usize, after: usize, image: bool) {
        self.brackets.push((self.items.len(), self.nodes.len()));
        let links = self.links.len();
        self.push(Item::Bracket {
            start,
            after,
            image,
            links,
        });
    }

    /// `text` as a link to `destination`, both in the text.
    fn link_text(&mut self, destination: Range<usize>, text: Range<usize>) {
        let link = self.links.len();
        let destination = self.map_range(&destination);
        self.links.push(Link {
            destination,
            title: None,
        });
        self.push(Item::LinkStart(link));
        self.push(Item::Text(text));
        self.push(Item::LinkEnd);
    }

    /// A `]` at `at`: a link or image when the last open bracket is active
    /// and an inline destination follows; else text. Answers where the scan
    /// goes on.
    fn close_bracket(&mut self, at: usize) -> usize {
        let Some((opener, bottom)) = self.brackets.pop() else {
            self.push(Item::Text(at..at + 1));
            return at + 1;
        };
        let Some(&Item::Bracket {
            start,
            after,
            image,
            links,
        }) = self.items.get(opener)
        else {
            self.push(Item::Text(at..at + 1));
            return at + 1;
        };
        let depth = self.brackets.len();
        let active = image || depth >= self.inactive_below;
        self.inactive_below = self.inactive_below.min(depth);
        let target = if active {
            self.destination(at + 1)
        } else {
            None
        };
        let Some((destination, title, end)) = target else {
            if let Some(item) = self.items.get_mut(opener) {
                *item = Item::Text(start..after);
            }
            self.push(Item::Text(at..at + 1));
            return at + 1;
        };
        let destination = self.map_range(&destination);
        let title = title.map(|title| self.map_range(&title));
        if image {
            // Its alt text is shown as written; what is inside goes: the
            // items after it, the links they named, and the emphasis
            // matched among them, which is the last matched, since no
            // bracket before this one has closed since it opened.
            self.items.truncate(opener);
            self.links.truncate(links);
            while self.pairs.last().is_some_and(|&(open, _, _)| open > opener) {
                self.pairs.pop();
            }
            self.truncate(bottom);
            let source = self.links.len();
            self.links.push(Link { destination, title });
            self.push(Item::Image {
                alt: after..at,
                source,
                markup: start..end,
            });
            return end;
        }
        let link = self.links.len();
        self.links.push(Link { destination, title });
        self.process_emphasis(bottom);
        // Links do not nest: an autolink inside gives way, and no
        // bracket before this one opens a link.
        for item in self.items.iter_mut().skip(opener + 1) {
            if matches!(item, Item::LinkStart(_) | Item::LinkEnd) {
                *item = Item::Gone;
            }
        }
        if let Some(item) = self.items.get_mut(opener) {
            *item = Item::LinkStart(link);
        }
        self.push(Item::LinkEnd);
        self.inactive_below = self.brackets.len();
        end
    }

    fn skip_space(&self, mut at: usize) -> usize {
        while matches!(self.byte(at), Some(b' ' | b'\t' | b'\n')) {
            at += 1;
        }
        at
    }

    /// An inline link's `(destination "title")` at `at`: the destination,
    /// the title, and where it ends.
    #[allow(clippy::type_complexity)]
    fn destination(&self, at: usize) -> Option<(Range<usize>, Option<Range<usize>>, usize)> {
        if self.byte(at) != Some(b'(') {
            return None;
        }
        let mut here = self.skip_space(at + 1);
        let destination = if self.byte(here) == Some(b'<') {
            let mut end = here + 1;
            loop {
                match self.byte(end)? {
                    b'>' => break,
                    b'<' | b'\n' => return None,
                    b'\\' if self.byte(end + 1) == Some(b'\n') => return None,
                    b'\\' => end += 2,
                    _ => end += 1,
                }
            }
            let destination = here + 1..end;
            here = end + 1;
            destination
        } else {
            let mut end = here;
            let mut depth = 0usize;
            while let Some(b) = self.byte(end) {
                match b {
                    b' ' | b'\t' | b'\n' => break,
                    b if b.is_ascii_control() => break,
                    b'\\' if self.byte(end + 1).is_some_and(|b| b.is_ascii_punctuation()) => {
                        end += 2;
                        continue;
                    }
                    b'(' => {
                        depth += 1;
                        if depth > MAX_PARENS {
                            return None;
                        }
                    }
                    b')' if depth == 0 => break,
                    b')' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            if depth != 0 {
                return None;
            }
            let destination = here..end;
            here = end;
            destination
        };
        let spaced = self.skip_space(here);
        let title = match self.byte(spaced) {
            // A title is one line here, so its range is the source's; one
            // in parentheses holds no unescaped one.
            Some(open @ (b'"' | b'\'' | b'(')) if spaced > here => {
                let close = if open == b'(' { b')' } else { open };
                let mut end = spaced + 1;
                loop {
                    match self.byte(end)? {
                        b if b == close => break,
                        b'\n' => return None,
                        b'(' if open == b'(' => return None,
                        b'\\' if self.byte(end + 1) == Some(b'\n') => return None,
                        b'\\' => end += 2,
                        _ => end += 1,
                    }
                }
                here = self.skip_space(end + 1);
                Some(spaced + 1..end)
            }
            _ => {
                here = spaced;
                None
            }
        };
        (self.byte(here) == Some(b')')).then_some((destination, title, here + 1))
    }

    /// An autolink `<scheme:...>` at `at`: where its `>` is.
    fn autolink(&self, at: usize) -> Option<usize> {
        let mut end = at + 1;
        if !self.byte(end)?.is_ascii_alphabetic() {
            return None;
        }
        while self
            .byte(end)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
        {
            end += 1;
        }
        let scheme = end - at - 1;
        if !(2..=MAX_SCHEME).contains(&scheme) || self.byte(end) != Some(b':') {
            return None;
        }
        loop {
            match self.byte(end)? {
                b'>' => return Some(end),
                b' ' | b'<' => return None,
                b if b.is_ascii_control() => return None,
                _ => end += 1,
            }
        }
    }

    /// A bare `http://` or `https://` URL at `at`, after a space or an
    /// opening mark: where it ends, before a backtick and, in a bracket, a
    /// `]`, trailing punctuation and unmatched closing parentheses left
    /// out.
    fn bare_url(&self, at: usize) -> Option<usize> {
        let rest = self.text.get(at..)?;
        let scheme = ["http://", "https://"]
            .into_iter()
            .find(|scheme| {
                rest.get(..scheme.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(scheme))
            })?
            .len();
        let before = self.before(at);
        if !(space(before) || matches!(before, Some('(' | '*' | '_' | '~' | '"' | '\''))) {
            return None;
        }
        let bracketed = !self.brackets.is_empty();
        let mut end = at;
        // Unmatched closing parentheses: closers less openers.
        let mut excess = 0isize;
        while let Some(b) = self.byte(end) {
            if b.is_ascii_whitespace()
                || b == b'<'
                || b == b'`'
                || b.is_ascii_control()
                || (b == b']' && bracketed)
            {
                break;
            }
            match b {
                b'(' => excess -= 1,
                b')' => excess += 1,
                _ => {}
            }
            end += 1;
        }
        while end > at + scheme {
            match self.byte(end - 1) {
                Some(
                    b'.' | b',' | b':' | b';' | b'!' | b'?' | b'"' | b'\'' | b'*' | b'_' | b'~',
                ) => end -= 1,
                Some(b')') if excess > 0 => {
                    excess -= 1;
                    end -= 1;
                }
                _ => break,
            }
        }
        (end > at + scheme).then_some(end)
    }

    fn delim(&self, node: usize) -> Option<&Delim> {
        match self.items.get(self.nodes.get(node)?.item)? {
            Item::Delim(delim) => Some(delim),
            _ => None,
        }
    }

    fn delim_mut(&mut self, node: usize) -> Option<&mut Delim> {
        let item = self.nodes.get(node)?.item;
        match self.items.get_mut(item)? {
            Item::Delim(delim) => Some(delim),
            _ => None,
        }
    }

    fn next(&self, node: usize) -> Option<usize> {
        self.nodes.get(node)?.next
    }

    fn prev(&self, node: usize) -> Option<usize> {
        self.nodes.get(node)?.prev
    }

    fn unlink(&mut self, node: usize) {
        let (prev, next) = match self.nodes.get(node) {
            Some(n) => (n.prev, n.next),
            None => return,
        };
        if let Some(p) = prev.and_then(|p| self.nodes.get_mut(p)) {
            p.next = next;
        }
        if let Some(n) = next.and_then(|n| self.nodes.get_mut(n)) {
            n.prev = prev;
        }
        if self.tail == Some(node) {
            self.tail = prev;
        }
    }

    /// Drops the delimiters from node `bottom` on: those inside a link or
    /// image, matched or not.
    fn truncate(&mut self, bottom: usize) {
        if bottom >= self.nodes.len() {
            return;
        }
        // Nodes before `bottom` are never unlinked by a pass over those
        // after it, so its first node's predecessor still stands.
        let before = self.prev(bottom);
        self.nodes.truncate(bottom);
        if let Some(node) = before.and_then(|b| self.nodes.get_mut(b)) {
            node.next = None;
        }
        self.tail = before;
    }

    /// CommonMark's emphasis over the delimiters from node `bottom` on,
    /// which are then dropped.
    fn process_emphasis(&mut self, bottom: usize) {
        // Per character, the closer's ability to open and its length mod 3:
        // the lowest node a failed search leaves an opener to be found at.
        let mut floors = [[[bottom; 3]; 2]; 3];
        let before = self.prev(bottom);
        let mut current = (bottom < self.nodes.len()).then_some(bottom);
        while let Some(closer) = current {
            let Some(c) = self.delim(closer) else {
                current = self.next(closer);
                continue;
            };
            if !c.close || c.count() == 0 {
                current = self.next(closer);
                continue;
            }
            let (ch, can_open, orig, count) = (c.ch, c.open, c.orig, c.count());
            let which = match ch {
                b'*' => 0,
                b'_' => 1,
                _ => 2,
            };
            let floor = floors
                .get(which)
                .and_then(|f| f.get(usize::from(can_open)))
                .and_then(|f| f.get(orig % 3))
                .copied()
                .unwrap_or(bottom);
            let mut search = self.prev(closer);
            let mut found = None;
            while let Some(candidate) = search {
                if candidate < floor || before == Some(candidate) {
                    break;
                }
                if let Some(o) = self.delim(candidate) {
                    let three = ch != b'~'
                        && (o.close || can_open)
                        && (o.orig + orig) % 3 == 0
                        && !(o.orig % 3 == 0 && orig % 3 == 0);
                    let tildes = ch == b'~' && o.count() != count;
                    if o.ch == ch && o.open && o.count() > 0 && !three && !tildes {
                        found = Some(candidate);
                        break;
                    }
                }
                search = self.prev(candidate);
            }
            match found {
                Some(opener) => {
                    let o_count = self.delim(opener).map_or(0, Delim::count);
                    let n = if ch == b'~' {
                        count
                    } else if o_count >= 2 && count >= 2 {
                        2
                    } else {
                        1
                    };
                    let style = match (ch, n) {
                        (b'~', _) => Style::STRIKE,
                        (_, 2) => Style::STRONG,
                        _ => Style::EMPHASIS,
                    };
                    if let Some(o) = self.delim_mut(opener) {
                        o.hi -= n;
                    }
                    if let Some(c) = self.delim_mut(closer) {
                        c.lo += n;
                    }
                    let (o_item, c_item) = match (self.nodes.get(opener), self.nodes.get(closer)) {
                        (Some(o), Some(c)) => (o.item, c.item),
                        _ => break,
                    };
                    self.pairs.push((o_item, c_item, style));
                    // The delimiters between are text now.
                    if let Some(o) = self.nodes.get_mut(opener) {
                        o.next = Some(closer);
                    }
                    if let Some(c) = self.nodes.get_mut(closer) {
                        c.prev = Some(opener);
                    }
                    if self.delim(opener).is_some_and(|o| o.count() == 0) {
                        self.unlink(opener);
                    }
                    if self.delim(closer).is_some_and(|c| c.count() == 0) {
                        current = self.next(closer);
                        self.unlink(closer);
                    }
                }
                None => {
                    if let Some(slot) = floors
                        .get_mut(which)
                        .and_then(|f| f.get_mut(usize::from(can_open)))
                        .and_then(|f| f.get_mut(orig % 3))
                    {
                        *slot = closer;
                    }
                    current = self.next(closer);
                    if !can_open {
                        self.unlink(closer);
                    }
                }
            }
        }
        self.truncate(bottom);
    }

    fn spans(&self) -> Vec<Span> {
        let mut deltas = vec![[0i32; 3]; self.items.len() + 1];
        for &(open, close, style) in &self.pairs {
            let bit = match style {
                Style::EMPHASIS => 0,
                Style::STRONG => 1,
                _ => 2,
            };
            if let Some(slot) = deltas.get_mut(open + 1).and_then(|d| d.get_mut(bit)) {
                *slot += 1;
            }
            if let Some(slot) = deltas.get_mut(close).and_then(|d| d.get_mut(bit)) {
                *slot -= 1;
            }
        }
        let mut counts = [0i32; 3];
        let mut link = None;
        let mut spans: Vec<Span> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            if let Some(delta) = deltas.get(index) {
                for (count, d) in counts.iter_mut().zip(delta) {
                    *count += d;
                }
            }
            let mut style = Style::PLAIN;
            for (count, bit) in counts
                .iter()
                .zip([Style::EMPHASIS, Style::STRONG, Style::STRIKE])
            {
                if *count > 0 {
                    style = style.with(bit);
                }
            }
            match item {
                Item::Text(range) => self.text_span(&mut spans, range, style, link),
                Item::Delim(delim) => {
                    self.text_span(&mut spans, &(delim.lo..delim.hi), style, link)
                }
                Item::Bracket { start, after, .. } => {
                    self.text_span(&mut spans, &(*start..*after), style, link)
                }
                Item::Code(range) => {
                    let mut from = range.start;
                    let inner = self.text.get(range.clone()).unwrap_or("");
                    for (offset, _) in inner.match_indices('\n') {
                        let at = range.start + offset;
                        self.code_span(&mut spans, from..at, style, link);
                        spans.push(self.break_span(SpanKind::SoftBreak, at));
                        from = at + 1;
                    }
                    self.code_span(&mut spans, from..range.end, style, link);
                }
                Item::LinkStart(l) => link = Some(*l),
                Item::LinkEnd => link = None,
                Item::Image {
                    alt,
                    source,
                    markup,
                } => {
                    // Its alt text up to a line's end.
                    let inner = self.text.get(alt.clone()).unwrap_or("");
                    let end = inner.find('\n').map_or(alt.end, |at| alt.start + at);
                    let markup = self.map_range(markup);
                    spans.push(Span {
                        kind: SpanKind::Image {
                            source: *source,
                            open: markup.start,
                            close: markup.end,
                        },
                        range: self.map_range(&(alt.start..end)),
                        style,
                        link,
                    });
                }
                Item::Soft(at) => spans.push(self.break_span(SpanKind::SoftBreak, *at)),
                Item::Hard(at) => spans.push(self.break_span(SpanKind::HardBreak, *at)),
                Item::Gone => {}
            }
        }
        spans
    }

    fn break_span(&self, kind: SpanKind, at: usize) -> Span {
        let at = self.map(at);
        Span {
            kind,
            range: at..at,
            style: Style::PLAIN,
            link: None,
        }
    }

    fn code_span(
        &self,
        spans: &mut Vec<Span>,
        range: Range<usize>,
        style: Style,
        link: Option<usize>,
    ) {
        if range.start < range.end {
            spans.push(Span {
                kind: SpanKind::Code,
                range: self.map_range(&range),
                style,
                link,
            });
        }
    }

    fn text_span(
        &self,
        spans: &mut Vec<Span>,
        range: &Range<usize>,
        style: Style,
        link: Option<usize>,
    ) {
        if range.start >= range.end {
            return;
        }
        let range = self.map_range(range);
        if let Some(last) = spans.last_mut() {
            if last.kind == SpanKind::Text
                && last.style == style
                && last.link == link
                && last.range.end == range.start
            {
                last.range.end = range.end;
                return;
            }
        }
        spans.push(Span {
            kind: SpanKind::Text,
            range,
            style,
            link,
        });
    }
}
