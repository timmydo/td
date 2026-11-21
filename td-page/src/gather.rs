//! A page's style sheets gathered (td-web/DESIGN.md §10, "Subresources"): its
//! `style` elements and `<link rel=stylesheet>`s in tree order, each
//! sheet's `@import`s at its head, fetched sheets decoded as they arrive
//! and `data:` ones at once, all on the render thread; and the order the
//! cascade takes them in, an import standing at its rule's place with the
//! media lists of its chain.

use crate::charset;
use crate::tree::{Sheet, Tree};
use td_alloc::{Budget, BudgetError, BudgetString, BudgetVec};
use td_charset::sniff;
use td_charset::{Decoder, Encoding};
use td_html::document::NodeId;
use td_url::{Reference, Url};

/// The most sheets a page links and imports, `data:` ones among them.
pub const MAX_SHEET_FETCHES: usize = 32;
/// How deep imports nest: a sheet a page links or holds is at depth
/// zero, what it imports at one.
pub const MAX_IMPORT_DEPTH: u8 = 4;
/// The most bytes of a sheet's text, decoded: td-css's per-input bound,
/// asked of the service as a fetched sheet's limit.
pub const MAX_SHEET: usize = td_css::MAX_INPUT_BYTES;
/// What a sheet asks for.
pub const ACCEPT: &str = "text/css,*/*;q=0.1";
/// The most bytes of text a page's fetched and `data:` sheets and its
/// imports' media lists hold together: td-css's bound for a set.
pub const MAX_TEXTS: usize = td_css::cascade::MAX_STYLE_BYTES;
/// The chain of media lists a sheet applies under: its `<link>`'s or
/// `<style>`'s, then each import's on the way to it.
const MAX_CHAIN: usize = MAX_IMPORT_DEPTH as usize + 1;

/// Where a sheet's text is.
enum Text {
    /// A `style` element's, in the tree.
    Element(NodeId),
    /// A fetched or `data:` sheet's, decoded.
    Own(BudgetString),
}

/// A sheet's own media list.
enum Media {
    None,
    /// The `media` attribute of a `style` or `link` element.
    Attribute(NodeId),
    /// An `@import`'s list, as written.
    Own(BudgetString),
}

enum State {
    /// To be fetched.
    Wanted,
    /// Being fetched.
    Asked,
    /// Its text, and the encoding its imports are decoded with by
    /// default.
    Ready { text: Text, encoding: Encoding },
    /// Not applied, and counted in the tally.
    Dropped,
}

struct Entry {
    /// The `link` or `style` element it is, at the top.
    node: Option<NodeId>,
    /// The sheet that imports it, by index.
    parent: Option<u32>,
    depth: u8,
    media: Media,
    /// The URL it is fetched from, as text, in the sheets term.
    url: Option<BudgetString>,
    state: State,
}

/// What became of the sheets a page linked and imported but did not
/// apply, for the status row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Tally {
    /// Fetched and refused: a failed fetch, an answer other than `2xx`,
    /// a type not `text/css`, mixed content, an address td-web does not
    /// fetch, a URL past `MAX_URL`, a `data:` URL that is not CSS or
    /// does not decode.
    pub failed: usize,
    /// Left out: past `MAX_SHEET_FETCHES`, `MAX_IMPORT_DEPTH` or the
    /// bytes, an import of a sheet importing it, or an import naming
    /// `layer` or `supports()`, which td-css does not apply.
    pub skipped: usize,
    /// Asked for or still to be, and not come by `STYLE_WAIT`.
    pub late: usize,
}

impl Tally {
    /// The sheets not applied but those that came late.
    pub fn not_applied(self) -> usize {
        self.failed.saturating_add(self.skipped)
    }
}

/// Why a sheet asked for was not taken.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refused {
    Failed,
    Skipped,
}

/// The most entries a page's sheets make: one for each sheet linked or
/// imported, and one for each `style` element importing, which is made
/// only with a sheet it imports taken.
const MAX_ENTRIES: usize = 2 * MAX_SHEET_FETCHES;
/// The longest URL a sheet is fetched from, as text: the longest td-url
/// parses.
pub const MAX_URL: usize = td_url::MAX_URL;
/// What a page's sheets hold beside their text: their entries, reserved
/// at once, room for each fetched one's URL, and the budgets' own blocks.
const BOOKKEEPING: usize = MAX_ENTRIES * std::mem::size_of::<Entry>()
    + MAX_SHEET_FETCHES * std::mem::size_of::<u32>()
    + MAX_SHEET_FETCHES * MAX_URL
    + 4 * 1024;
/// The most a page's sheets hold, of the sheets term: their text and
/// their bookkeeping.
pub const MAX_HELD: usize = MAX_TEXTS + BOOKKEEPING;

/// A page's sheets as they are gathered: an entry for each sheet linked
/// or imported and each `style` element that imports, in the order they
/// were found; the page's other `style` elements are its tree's. In
/// cascade order through `each`.
pub struct Sheets {
    entries: BudgetVec<Entry>,
    /// The entries an import made, in the order they were found.
    imports: BudgetVec<u32>,
    /// What the entries, the fetched and `data:` sheets' text and URLs
    /// and the imports' media lists are held in, the sheets term's: at
    /// most `MAX_TEXTS` of text beside `BOOKKEEPING`.
    texts: Budget,
    /// The page's budget, which a `data:` URL's decoding scratch is
    /// charged to while it is decoded.
    page: Budget,
    /// The page's encoding: a linked sheet's by default.
    encoding: Encoding,
    /// The page is `https`: an `http` sheet under it is mixed content.
    https: bool,
    /// The sheets linked and imported so far.
    taken: usize,
    tally: Tally,
}

impl Sheets {
    /// Whether `tree` names a sheet to gather: a `link`, or a `style`
    /// element that imports. A page that names none holds no `Sheets`.
    pub fn needed(tree: &Tree) -> bool {
        tree.sheets().iter().any(|&id| match tree.sheet(id) {
            Some(Sheet::Linked { .. }) => true,
            Some(Sheet::Inline { text, .. }) => imports_in(text) > 0 || unapplied_in(text) > 0,
            None => false,
        })
    }

    /// The sheets `tree` names, its `style` elements' imports found and
    /// its `data:` sheets decoded: `url` is the page's, `encoding` its
    /// text's. What is to be fetched is `wanted`.
    pub fn new(
        tree: &Tree,
        url: &Url,
        encoding: Encoding,
        page: &Budget,
    ) -> Result<Self, BudgetError> {
        let base = tree.base().and_then(|href| Url::join(url, href).ok());
        let base = base.as_ref().unwrap_or(url);
        let texts = page.child(MAX_HELD.saturating_add(Budget::metadata_bytes()))?;
        let mut sheets = Self {
            entries: td_alloc::BudgetVec::with_capacity(&texts, MAX_ENTRIES)?,
            imports: td_alloc::BudgetVec::with_capacity(&texts, MAX_SHEET_FETCHES)?,
            texts,
            page: page.try_clone()?,
            encoding,
            https: url.https,
            taken: 0,
            tally: Tally::default(),
        };
        for &id in tree.sheets() {
            let media = match tree.media(id) {
                Some(_) => Media::Attribute(id),
                None => Media::None,
            };
            match tree.sheet(id) {
                // One that imports nothing is read from the tree.
                Some(Sheet::Inline { text, .. }) if imports_in(text) > 0 => {
                    // Past the count, read from the tree, its imports
                    // left out.
                    let made = (sheets.taken < MAX_SHEET_FETCHES)
                        .then(|| {
                            sheets
                                .add(Some(id), None, 0, media, None, State::Asked)
                                .ok()
                        })
                        .flatten();
                    let Some(at) = made else {
                        sheets.tally.skipped += imports_in(text) + unapplied_in(text);
                        continue;
                    };
                    sheets.imports_of(at, 0, text, Some(base), encoding)?;
                    sheets.settle(
                        at,
                        State::Ready {
                            text: Text::Element(id),
                            encoding,
                        },
                    );
                }
                Some(Sheet::Linked { href, .. }) => {
                    sheets.take(Some(id), None, 0, media, href, Some(base), encoding)?;
                }
                // Read from the tree, its imports td-css does not apply
                // counted.
                Some(Sheet::Inline { text, .. }) => sheets.tally.skipped += unapplied_in(text),
                None => {}
            }
        }
        Ok(sheets)
    }

    fn add(
        &mut self,
        node: Option<NodeId>,
        parent: Option<u32>,
        depth: u8,
        media: Media,
        url: Option<BudgetString>,
        state: State,
    ) -> Result<u32, BudgetError> {
        // Within the reservation, never grown.
        if self.entries.len() >= MAX_ENTRIES {
            return Err(BudgetError::Limit);
        }
        let at = u32::try_from(self.entries.len()).map_err(|_| BudgetError::Limit)?;
        self.entries.try_push(Entry {
            node,
            parent,
            depth,
            media,
            url,
            state,
        })?;
        if parent.is_some() {
            self.imports.try_push(at)?;
        }
        Ok(at)
    }

    fn settle(&mut self, at: u32, state: State) {
        if let Some(entry) = self.entries.get_mut(at as usize) {
            entry.state = state;
        }
    }

    fn refuse(&mut self, why: Refused) {
        match why {
            Refused::Failed => self.tally.failed += 1,
            Refused::Skipped => self.tally.skipped += 1,
        }
    }

    /// Whether `url` is the URL of `parent` or of a sheet importing it.
    fn cycle(&self, parent: Option<u32>, url: &str) -> bool {
        let mut at = parent;
        while let Some(index) = at {
            let Some(entry) = self.entries.get(index as usize) else {
                return false;
            };
            if entry.url.as_ref().map(BudgetString::as_str) == Some(url) {
                return true;
            }
            at = entry.parent;
        }
        false
    }

    /// A linked or imported sheet, `href` as written: a `data:` one
    /// decoded at once, another to be fetched, joined against `base`
    /// (none: only an absolute URL is taken), or refused.
    #[allow(clippy::too_many_arguments)]
    fn take(
        &mut self,
        node: Option<NodeId>,
        parent: Option<u32>,
        depth: u8,
        media: Media,
        href: &str,
        base: Option<&Url>,
        encoding: Encoding,
    ) -> Result<(), BudgetError> {
        if self.taken >= MAX_SHEET_FETCHES {
            self.refuse(Refused::Skipped);
            return Ok(());
        }
        self.taken += 1;
        if let Some(data) = data_url(href, &self.page) {
            let data = match data {
                Ok(Some(data)) if data.css => data,
                // Not CSS, or not decoded.
                Ok(_) => {
                    self.refuse(Refused::Failed);
                    return Ok(());
                }
                Err(_) => {
                    self.refuse(Refused::Skipped);
                    return Ok(());
                }
            };
            let label = data.label.as_ref().map(charset::Label::as_bytes);
            let Ok(at) = self.add(node, parent, depth, media, None, State::Asked) else {
                self.refuse(Refused::Skipped);
                return Ok(());
            };
            return self.decoded(at, depth, &data.bytes, label, None, encoding);
        }
        let reference = match base {
            Some(base) => td_url::resolve(base, href),
            None => Url::parse(href.trim()).map(|url| Reference::Fetch {
                url,
                fragment: None,
            }),
        };
        let url = match reference {
            Ok(Reference::Fetch { url, .. }) => url,
            Ok(Reference::Other { .. }) | Err(_) => {
                self.refuse(Refused::Failed);
                return Ok(());
            }
        };
        let https = url.https;
        let text = url.text();
        drop(url);
        if (self.https && !https) || text.len() > MAX_URL {
            self.refuse(Refused::Failed);
            return Ok(());
        }
        if self.cycle(parent, &text) {
            self.refuse(Refused::Skipped);
            return Ok(());
        }
        let added = BudgetString::copy(&self.texts, &text)
            .and_then(|url| self.add(node, parent, depth, media, Some(url), State::Wanted));
        if added.is_err() {
            self.refuse(Refused::Skipped);
        }
        Ok(())
    }

    /// The imports at the head of sheet `at`'s `text`, each taken in turn.
    fn imports_of(
        &mut self,
        at: u32,
        depth: u8,
        text: &str,
        base: Option<&Url>,
        encoding: Encoding,
    ) -> Result<(), BudgetError> {
        let mut rest = text;
        while let Some((import, after)) = next_import(rest) {
            rest = after;
            let import = match import {
                Prelude::Import(import) => import,
                Prelude::Unapplied => {
                    self.refuse(Refused::Skipped);
                    continue;
                }
                Prelude::Malformed => continue,
            };
            // Counted before anything of it is copied.
            if depth >= MAX_IMPORT_DEPTH || self.taken >= MAX_SHEET_FETCHES {
                self.refuse(Refused::Skipped);
                continue;
            }
            let media = if import.media.is_empty() {
                Media::None
            } else {
                match BudgetString::copy(&self.texts, import.media) {
                    Ok(list) => Media::Own(list),
                    Err(_) => {
                        self.refuse(Refused::Skipped);
                        continue;
                    }
                }
            };
            let Ok(href) = unescape(import.url, &self.texts) else {
                self.refuse(Refused::Skipped);
                continue;
            };
            self.take(None, Some(at), depth + 1, media, &href, base, encoding)?;
        }
        Ok(())
    }

    /// `body` decoded as sheet `at`'s text and its imports taken; one past
    /// the bounds left out.
    fn decoded(
        &mut self,
        at: u32,
        depth: u8,
        body: &[u8],
        label: Option<&[u8]>,
        base: Option<&Url>,
        environment: Encoding,
    ) -> Result<(), BudgetError> {
        let Ok(Some((text, encoding))) = css_text(body, label, environment, &self.texts) else {
            self.settle(at, State::Dropped);
            self.refuse(Refused::Skipped);
            return Ok(());
        };
        self.imports_of(at, depth, &text, base, encoding)?;
        self.settle(
            at,
            State::Ready {
                text: Text::Own(text),
                encoding,
            },
        );
        Ok(())
    }

    /// The first sheet still to be fetched, and its URL's text.
    pub fn wanted(&self) -> Option<(usize, &str)> {
        self.entries
            .iter()
            .enumerate()
            .find(|(_, entry)| matches!(entry.state, State::Wanted))
            .and_then(|(at, entry)| Some((at, entry.url.as_ref()?.as_str())))
    }

    /// Sheet `at` is being fetched.
    pub fn asked(&mut self, at: usize) {
        if let Some(entry) = self.entries.get_mut(at) {
            if matches!(entry.state, State::Wanted) {
                entry.state = State::Asked;
            }
        }
    }

    /// Sheet `at` was not had, and why.
    pub fn refused(&mut self, at: usize, why: Refused) {
        let Some(entry) = self.entries.get_mut(at) else {
            return;
        };
        if matches!(entry.state, State::Wanted | State::Asked) {
            entry.state = State::Dropped;
            self.refuse(why);
        }
    }

    /// Sheet `at` came from `answered` as `body`, `content_type` its
    /// type: decoded, its imports taken.
    pub fn arrived(
        &mut self,
        at: usize,
        answered: &Url,
        content_type: Option<&str>,
        body: &[u8],
    ) -> Result<(), BudgetError> {
        let Some(entry) = self.entries.get(at) else {
            return Ok(());
        };
        if !matches!(entry.state, State::Wanted | State::Asked) {
            return Ok(());
        }
        let depth = entry.depth;
        // A linked sheet's default is the page's encoding, an imported
        // one's the sheet importing it (CSS Syntax §3.2).
        let environment = entry
            .parent
            .and_then(|parent| match self.entries.get(parent as usize)?.state {
                State::Ready { encoding, .. } => Some(encoding),
                _ => None,
            })
            .unwrap_or(self.encoding);
        let label = content_type.and_then(charset::label);
        let label = label.as_ref().map(charset::Label::as_bytes);
        let at = u32::try_from(at).map_err(|_| BudgetError::Limit)?;
        self.decoded(at, depth, body, label, Some(answered), environment)
    }

    /// Whether a sheet is still to be fetched or being fetched.
    pub fn unsettled(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.state, State::Wanted | State::Asked))
    }

    /// The sheets not yet come are late.
    pub fn finish(&mut self) {
        let mut late = 0usize;
        for entry in self.entries.iter_mut() {
            if matches!(entry.state, State::Wanted | State::Asked) {
                entry.state = State::Dropped;
                late += 1;
            }
        }
        self.tally.late = self.tally.late.saturating_add(late);
    }

    /// Each of the page's own sheets in cascade order, `tree`'s `style`
    /// elements and `link`s in tree order with each sheet's imports before
    /// it, given to `visit` with its chain of media lists.
    pub fn each<'a, E>(
        &'a self,
        tree: &'a Tree,
        mut visit: impl FnMut(Given<'a>) -> Result<(), E>,
    ) -> Result<(), E> {
        // Entries at the top were found in tree order.
        let mut tops = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.parent.is_none())
            .peekable();
        for &id in tree.sheets() {
            if let Some(&(at, _)) = tops.peek().filter(|(_, entry)| entry.node == Some(id)) {
                tops.next();
                self.emit(tree, at, 0, &mut visit)?;
                continue;
            }
            if let Some(Sheet::Inline { text, media }) = tree.sheet(id) {
                visit(Given::inline(text, media))?;
            }
        }
        Ok(())
    }

    /// Sheet `at`'s imports, each before its own, then `at`'s.
    fn emit<'a, E>(
        &'a self,
        tree: &'a Tree,
        at: usize,
        depth: u8,
        visit: &mut impl FnMut(Given<'a>) -> Result<(), E>,
    ) -> Result<(), E> {
        if depth <= MAX_IMPORT_DEPTH {
            for &child in self.imports.iter() {
                let mine = self
                    .entries
                    .get(child as usize)
                    .is_some_and(|entry| entry.parent.map(|p| p as usize) == Some(at));
                if mine {
                    self.emit(tree, child as usize, depth + 1, visit)?;
                }
            }
        }
        let Some(entry) = self.entries.get(at) else {
            return Ok(());
        };
        let text = match &entry.state {
            State::Ready {
                text: Text::Element(id),
                ..
            } => match tree.sheet(*id) {
                Some(Sheet::Inline { text, .. }) => text,
                _ => return Ok(()),
            },
            State::Ready {
                text: Text::Own(text),
                ..
            } => text.as_str(),
            _ => return Ok(()),
        };
        let mut given = Given {
            text,
            lists: [""; MAX_CHAIN],
            count: 0,
        };
        // Innermost first, then turned about.
        let mut on = Some(entry);
        while let Some(entry) = on {
            let list = match &entry.media {
                Media::None => None,
                Media::Attribute(id) => tree.media(*id),
                Media::Own(list) => Some(list.as_str()),
            };
            if let Some(list) = list {
                if let Some(slot) = given.lists.get_mut(given.count) {
                    *slot = list;
                    given.count += 1;
                }
            }
            on = entry
                .parent
                .and_then(|parent| self.entries.get(parent as usize));
        }
        if let Some(chain) = given.lists.get_mut(..given.count) {
            chain.reverse();
        }
        visit(given)
    }

    /// The sheets linked and imported. For tests and measurements.
    pub fn taken(&self) -> usize {
        self.taken
    }

    /// What became of the sheets not applied.
    pub fn tally(&self) -> Tally {
        self.tally
    }

    /// The bytes they hold, which the sheets term counts.
    pub fn bytes(&self) -> usize {
        self.texts.used()
    }

    /// The fetched and `data:` sheets' text.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter_map(|entry| match &entry.state {
            State::Ready {
                text: Text::Own(text),
                ..
            } => Some(text.as_str()),
            _ => None,
        })
    }
}

/// One of the page's own sheets as the cascade takes it: its text and
/// its chain of media lists, outermost first.
#[derive(Clone, Copy, Debug)]
pub struct Given<'a> {
    pub text: &'a str,
    lists: [&'a str; MAX_CHAIN],
    count: usize,
}

impl<'a> Given<'a> {
    /// A sheet under at most one media list: a `style` element's.
    pub fn inline(text: &'a str, media: Option<&'a str>) -> Self {
        let mut given = Self {
            text,
            lists: [""; MAX_CHAIN],
            count: 0,
        };
        if let (Some(media), Some(slot)) = (media, given.lists.first_mut()) {
            *slot = media;
            given.count = 1;
        }
        given
    }

    pub fn lists(&self) -> &[&'a str] {
        self.lists.get(..self.count).unwrap_or_default()
    }

    /// The bytes td-css counts it as: its text and each media list, a
    /// list at least one.
    pub fn bytes(&self) -> usize {
        self.lists().iter().fold(self.text.len(), |sum, list| {
            sum.saturating_add(list.len().max(1))
        })
    }
}

/// An `@import` at a sheet's head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Import<'a> {
    /// Its URL as written, escapes not yet undone.
    url: &'a str,
    /// Its media list as written, outer white space trimmed.
    media: &'a str,
}

/// CSS's white space.
fn space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)
}

/// From `at`, past white space and comments, and `<!--` and `-->` at a
/// sheet's top level.
fn skip(bytes: &[u8], mut at: usize, top: bool) -> usize {
    loop {
        let Some(rest) = bytes.get(at..) else {
            return at;
        };
        if rest.first().copied().is_some_and(space) {
            at += 1;
        } else if rest.starts_with(b"/*") {
            at = comment_end(bytes, at);
        } else if top && rest.starts_with(b"<!--") {
            at += 4;
        } else if top && rest.starts_with(b"-->") {
            at += 3;
        } else {
            return at;
        }
    }
}

/// Past the comment starting at `at`; its end, or the text's.
fn comment_end(bytes: &[u8], at: usize) -> usize {
    let from = at + 2;
    bytes
        .get(from..)
        .and_then(|rest| rest.windows(2).position(|pair| pair == b"*/"))
        .map_or(bytes.len(), |end| from + end + 2)
}

/// Past the string whose quote is at `at`: its closing quote's index, or
/// the text's end; for a newline unescaped in it, a bad string, the
/// newline's index, where the tokenizer goes on (CSS Syntax §4.3.5).
fn string_end(bytes: &[u8], at: usize) -> Result<usize, usize> {
    let quote = bytes.get(at).copied().ok_or(at)?;
    let mut on = at + 1;
    while let Some(&byte) = bytes.get(on) {
        match byte {
            b'\\' => on += 2,
            b'\n' | b'\r' | 0x0c => return Err(on),
            _ if byte == quote => return Ok(on),
            _ => on += 1,
        }
    }
    Ok(bytes.len())
}

/// How a statement from `at` ends.
enum End {
    /// At this `;`.
    Semicolon(usize),
    /// At a `{`: a block.
    Block,
    /// With the text.
    Eof,
}

fn statement(bytes: &[u8], mut at: usize) -> End {
    let mut depth = 0usize;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            b'"' | b'\'' => {
                at = string_end(bytes, at).map_or_else(|newline| newline, |end| end + 1);
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => at = comment_end(bytes, at),
            b'\\' => at += 2,
            b'(' | b'[' => {
                depth += 1;
                at += 1;
            }
            b'{' if depth > 0 => {
                depth += 1;
                at += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                at += 1;
            }
            b'{' => return End::Block,
            b';' if depth == 0 => return End::Semicolon(at),
            _ => at += 1,
        }
    }
    End::Eof
}

/// Whether `byte` continues a CSS name.
fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte >= 0x80
}

/// The next `@import` at the head of `text` (CSS Cascade §2: after any
/// `@charset` and `@layer` statements, before any other rule), read, and
/// the text after it; `None` once the head ends.
fn next_import(text: &str) -> Option<(Prelude<'_>, &str)> {
    let bytes = text.as_bytes();
    let mut from = 0;
    loop {
        let at = skip(bytes, from, true);
        let rest = bytes.get(at..)?;
        if rest.first() != Some(&b'@') {
            return None;
        }
        let name_end = at + 1 + rest.iter().skip(1).take_while(|&&b| name_byte(b)).count();
        let name = bytes.get(at + 1..name_end)?;
        let (end, after) = match statement(bytes, name_end) {
            End::Semicolon(end) => (end, end + 1),
            End::Eof => (bytes.len(), bytes.len()),
            // A block ends the head: an `@layer` block, an import with
            // one, which is invalid, or any other rule.
            End::Block => return None,
        };
        if name.eq_ignore_ascii_case(b"charset") || name.eq_ignore_ascii_case(b"layer") {
            from = after;
            continue;
        }
        if !name.eq_ignore_ascii_case(b"import") {
            return None;
        }
        let prelude = text.get(name_end..end)?;
        return Some((import(prelude), text.get(after..)?));
    }
}

/// How many sheets `text` imports at its head.
fn imports_in(text: &str) -> usize {
    preludes_in(text, |prelude| matches!(prelude, Prelude::Import(_)))
}

/// How many imports at `text`'s head name `layer` or `supports()`.
fn unapplied_in(text: &str) -> usize {
    preludes_in(text, |prelude| matches!(prelude, Prelude::Unapplied))
}

fn preludes_in(text: &str, counted: impl Fn(&Prelude<'_>) -> bool) -> usize {
    let mut count = 0;
    let mut rest = text;
    while let Some((prelude, after)) = next_import(rest) {
        count += usize::from(counted(&prelude));
        rest = after;
    }
    count
}

/// An `@import`'s prelude, read.
enum Prelude<'a> {
    Import(Import<'a>),
    /// Naming `layer` or `supports()`, which td-css does not apply: a
    /// sheet left out.
    Unapplied,
    /// Not an import at all: an invalid rule, passed over.
    Malformed,
}

/// An `@import`'s prelude read: its URL, then its media list, unless it
/// names `layer` or `supports()`.
fn import(prelude: &str) -> Prelude<'_> {
    import_of(prelude).unwrap_or(Prelude::Malformed)
}

fn import_of(prelude: &str) -> Option<Prelude<'_>> {
    let bytes = prelude.as_bytes();
    let at = skip(bytes, 0, false);
    let (url, after) = match bytes.get(at)? {
        b'"' | b'\'' => {
            let end = string_end(bytes, at).ok()?;
            (prelude.get(at + 1..end)?, (end + 1).min(bytes.len()))
        }
        _ => {
            let name = bytes.get(at..at + 4)?;
            if !name.eq_ignore_ascii_case(b"url(") {
                return None;
            }
            let mut on = at + 4;
            while bytes.get(on).copied().is_some_and(space) {
                on += 1;
            }
            let (url, end) = match bytes.get(on) {
                Some(b'"' | b'\'') => {
                    let end = string_end(bytes, on).ok()?;
                    let url = prelude.get(on + 1..end)?;
                    (url, skip(bytes, end + 1, false))
                }
                _ => {
                    let start = on;
                    while let Some(&byte) = bytes.get(on) {
                        match byte {
                            b')' => break,
                            b'\\' => on += 2,
                            b'"' | b'\'' | b'(' | 0..=0x08 | 0x0b | 0x0e..=0x1f | 0x7f => {
                                return None
                            }
                            _ if space(byte) => break,
                            _ => on += 1,
                        }
                    }
                    let url = prelude.get(start..on.min(bytes.len()))?;
                    while bytes.get(on).copied().is_some_and(space) {
                        on += 1;
                    }
                    (url, on)
                }
            };
            match bytes.get(end) {
                Some(b')') => (url, end + 1),
                // `url(` left open to the end of the rule.
                None => (url, end),
                Some(_) => return None,
            }
        }
    };
    let rest = prelude.get(skip(bytes, after, false)..)?;
    let named = |word: &[u8]| {
        rest.as_bytes()
            .get(..word.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(word))
            && !rest
                .as_bytes()
                .get(word.len())
                .copied()
                .is_some_and(name_byte)
    };
    if named(b"layer") || named(b"supports") {
        return Some(Prelude::Unapplied);
    }
    Some(Prelude::Import(Import {
        url,
        media: rest.trim_matches(|c: char| c.is_ascii() && space(c as u8)),
    }))
}

/// `raw`, a URL as a sheet wrote it, its escapes undone (CSS Syntax §4.3.7):
/// a hexadecimal one and one space after it, a line continuation, any
/// other character escaped as itself.
fn unescape(raw: &str, memory: &Budget) -> Result<BudgetString, BudgetError> {
    let mut out = td_alloc::BudgetString::with_capacity(memory, raw.len())?;
    let mut scalars = raw.chars().peekable();
    while let Some(c) = scalars.next() {
        if c != '\\' {
            out.try_push(c)?;
            continue;
        }
        let Some(&next) = scalars.peek() else {
            break;
        };
        if next == '\n' || next == '\x0c' {
            scalars.next();
            continue;
        }
        if next == '\r' {
            scalars.next();
            if scalars.peek() == Some(&'\n') {
                scalars.next();
            }
            continue;
        }
        if !next.is_ascii_hexdigit() {
            scalars.next();
            out.try_push(next)?;
            continue;
        }
        let mut value = 0u32;
        for _ in 0..6 {
            match scalars.peek().and_then(|c| c.to_digit(16)) {
                Some(digit) => {
                    value = value * 16 + digit;
                    scalars.next();
                }
                None => break,
            }
        }
        if scalars
            .peek()
            .is_some_and(|&c| c.is_ascii() && space(c as u8))
        {
            scalars.next();
        }
        let scalar = match value {
            0 => '\u{fffd}',
            value => char::from_u32(value).unwrap_or('\u{fffd}'),
        };
        out.try_push(scalar)?;
    }
    Ok(out)
}

/// A `data:` URL's payload, decoded.
struct Data {
    /// Its type is `text/css`.
    css: bool,
    label: Option<charset::Label>,
    bytes: BudgetVec<u8>,
}

/// `href` read as a `data:` URL (WHATWG Fetch, "data: URL processor"):
/// its media type, and its payload percent-decoded and, with `;base64`,
/// base64-decoded, its fragment left out as the URL parser leaves it;
/// `None` when it is no `data:` URL; within, none when it does not
/// decode, an error when it cannot be held. Its scratch is `memory`'s
/// while it is decoded, and so is what it decodes to, until copied.
fn data_url(href: &str, memory: &Budget) -> Option<Result<Option<Data>, BudgetError>> {
    let href = href.trim_matches(|c: char| c.is_ascii() && (space(c as u8) || c < ' '));
    let scheme = href.get(..5)?;
    if !scheme.eq_ignore_ascii_case("data:") {
        return None;
    }
    let rest = href.get(5..)?;
    let rest = rest.split('#').next().unwrap_or(rest);
    let Some(comma) = rest.find(',') else {
        return Some(Ok(None));
    };
    let (kind, payload) = (rest.get(..comma)?, rest.get(comma + 1..)?);
    let kind = kind.trim_matches(|c: char| c.is_ascii_whitespace());
    let (kind, base64) = match kind.len().checked_sub(7).and_then(|n| kind.get(n..)) {
        Some(tail) if tail.eq_ignore_ascii_case(";base64") => {
            (kind.get(..kind.len() - 7).unwrap_or(""), true)
        }
        _ => (kind, false),
    };
    let essence = kind
        .split(';')
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c.is_ascii_whitespace());
    let css = essence.eq_ignore_ascii_case("text/css");
    let label = charset::label(kind);
    let decoded = decode_data(payload, base64, memory);
    Some(decoded.map(|bytes| bytes.map(|bytes| Data { css, label, bytes })))
}

/// `href`'s payload as a `data:` URL, whatever its type: `None` when it
/// is no `data:` URL; within, none when it does not decode, an error when
/// it cannot be held. A `data:` picture's (td-web/DESIGN.md §10, "Requests").
pub fn data(href: &str, memory: &Budget) -> Option<Result<Option<BudgetVec<u8>>, BudgetError>> {
    data_url(href, memory).map(|data| data.map(|data| data.map(|data| data.bytes)))
}

fn hex(byte: u8) -> Option<u8> {
    char::from(byte)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// A `data:` URL's payload percent-decoded, then base64-decoded when it
/// says so, its ASCII white space dropped first (WHATWG's forgiving
/// base64); none when that does not decode.
fn decode_data(
    payload: &str,
    base64: bool,
    memory: &Budget,
) -> Result<Option<BudgetVec<u8>>, BudgetError> {
    let bytes = payload.as_bytes();
    let mut out = td_alloc::BudgetVec::with_capacity(memory, bytes.len())?;
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        let pair = bytes
            .get(at + 1)
            .copied()
            .and_then(hex)
            .zip(bytes.get(at + 2).copied().and_then(hex));
        match (byte, pair) {
            (b'%', Some((high, low))) => {
                out.try_push(high * 16 + low)?;
                at += 3;
            }
            _ => {
                out.try_push(byte)?;
                at += 1;
            }
        }
    }
    if !base64 {
        return Ok(Some(out));
    }
    let mut decoded = td_alloc::BudgetVec::with_capacity(memory, out.len() / 4 * 3 + 3)?;
    let mut digits = 0usize;
    let mut held = 0u32;
    let mut padding = 0usize;
    for &byte in out.iter() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            _ if space(byte) => continue,
            _ => return Ok(None),
        };
        if padding > 0 {
            return Ok(None);
        }
        held = held << 6 | u32::from(value);
        digits += 1;
        if digits.is_multiple_of(4) {
            for shift in [16, 8, 0] {
                decoded.try_push((held >> shift) as u8)?;
            }
            held = 0;
        }
    }
    let left = digits % 4;
    if padding > 2 || (padding > 0 && !(left + padding).is_multiple_of(4)) || left == 1 {
        return Ok(None);
    }
    match left {
        2 => decoded.try_push((held >> 4) as u8)?,
        3 => {
            decoded.try_push((held >> 10) as u8)?;
            decoded.try_push((held >> 2) as u8)?;
        }
        _ => {}
    }
    Ok(Some(decoded))
}

/// The encoding an `@charset` rule at the very start of `bytes` names
/// (CSS Syntax §3.2): the first 1024 bytes beginning `@charset "`, a
/// label of no `"` or `;`, then `";`; UTF-8 for a UTF-16 one, none for a
/// label td-charset does not know.
fn charset_rule(bytes: &[u8]) -> Option<Encoding> {
    let window = bytes.get(..1024).unwrap_or(bytes);
    let rest = window.strip_prefix(b"@charset \"")?;
    let end = rest.iter().position(|&b| b == b'"' || b == b';')?;
    if rest.get(end..end + 2) != Some(b"\";".as_slice()) {
        return None;
    }
    let label = rest.get(..end)?;
    match Encoding::for_label(label)? {
        Encoding::Utf16Be | Encoding::Utf16Le => Some(Encoding::Utf8),
        encoding => Some(encoding),
    }
}

/// A sheet's `body` decoded as CSS Syntax §3.2 orders it: a byte order
/// mark, the `content-type`'s `label`, an `@charset` rule, the
/// `environment`, the page's or the importing sheet's encoding. At most
/// `MAX_SHEET` bytes of UTF-8 in `memory`, else `None`, counted before
/// the string is had.
fn css_text(
    body: &[u8],
    label: Option<&[u8]>,
    environment: Encoding,
    memory: &Budget,
) -> Result<Option<(BudgetString, Encoding)>, BudgetError> {
    let fallback = charset_rule(body).unwrap_or(environment);
    let chosen = sniff::sniff(body, label, false, fallback);
    let bytes = body.get(chosen.bom_len..).unwrap_or_default();
    let encoding = chosen.encoding;
    if encoding == Encoding::Utf8 {
        if let Ok(text) = std::str::from_utf8(bytes) {
            if text.len() > MAX_SHEET {
                return Ok(None);
            }
            return BudgetString::copy(memory, text).map(|text| Some((text, encoding)));
        }
    }
    let mut length = 0usize;
    each_scalar(bytes, encoding, |c| {
        length = length.saturating_add(c.len_utf8());
        Ok(())
    })?;
    if length > MAX_SHEET {
        return Ok(None);
    }
    let mut text = td_alloc::BudgetString::with_capacity(memory, length)?;
    each_scalar(bytes, encoding, |c| text.try_push(c))?;
    Ok(Some((text, encoding)))
}

/// Each scalar `bytes` decode to from `encoding`, malformed ones replaced.
fn each_scalar(
    bytes: &[u8],
    encoding: Encoding,
    mut each: impl FnMut(char) -> Result<(), BudgetError>,
) -> Result<(), BudgetError> {
    let mut decoder = Decoder::new(encoding);
    for &byte in bytes {
        // Called by path: the allocation guard counts every `.push(`.
        let Ok(scalars) = Decoder::push(&mut decoder, byte) else {
            break;
        };
        for c in scalars.chars() {
            each(c)?;
        }
    }
    for c in decoder.finish().chars() {
        each(c)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
mod tests {
    use super::*;
    use crate::tree::Source;

    fn imports(text: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some((import, after)) = next_import(rest) {
            rest = after;
            if let Prelude::Import(import) = import {
                out.push((import.url.to_string(), import.media.to_string()));
            }
        }
        out
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn imports_are_read_at_the_head_only() {
        assert_eq!(
            imports(
                "@charset \"utf-8\";\n/* c */ <!-- @layer base, x;\n\
                 @import url(a.css);@IMPORT 'b.css' screen and (min-width: 1px) ;\
                 @import url( \"c;d.css\" ) print;\
                 @import url(e.css) layer(x) screen; @import \"f.css\" supports(display:grid);\
                 @import \"g.css\" layer;\
                 @import url(h\\).css)/**/ ;\
                 p{} @import 'late.css';"
            ),
            pairs(&[
                ("a.css", ""),
                ("b.css", "screen and (min-width: 1px)"),
                ("c;d.css", "print"),
                ("h\\).css", ""),
            ])
        );
        // A malformed import is passed over; a block or any other rule
        // ends the head.
        assert_eq!(
            imports("@import 3; @import url(a b); @import 'ok'; @media x {} @import 'no';"),
            pairs(&[("ok", "")])
        );
        assert_eq!(imports("@layer x { } @import 'no';"), pairs(&[]));
        assert_eq!(imports("@import 'last'"), pairs(&[("last", "")]));
        assert_eq!(imports("@import url(open"), pairs(&[("open", "")]));
        // A bad string ends at its newline, as CSS's tokenizer ends it.
        assert_eq!(imports("@import 'a\n; @import 'c';"), pairs(&[("c", "")]));
        assert_eq!(imports("@font-face{} @import 'x';"), pairs(&[]));
    }

    #[test]
    fn escapes_in_a_url_are_undone() {
        let memory = Budget::new(1 << 20).unwrap();
        let undone = |raw| unescape(raw, &memory).unwrap().as_str().to_string();
        assert_eq!(undone("a\\).css"), "a).css");
        assert_eq!(undone("\\61 b\\000063"), "abc");
        assert_eq!(undone("x\\\ny"), "xy");
        assert_eq!(undone("\\0 z"), "\u{fffd}z");
        assert_eq!(undone("\\110000"), "\u{fffd}");
    }

    fn data(href: &str) -> Option<(bool, Option<String>, Vec<u8>)> {
        let memory = Budget::new(1 << 20).unwrap();
        let data = data_url(href, &memory)?.ok()??;
        let label = data
            .label
            .map(|label| String::from_utf8(label.as_bytes().to_vec()).unwrap());
        Some((data.css, label, data.bytes.as_slice().to_vec()))
    }

    #[test]
    fn data_urls_are_percent_and_base64_decoded() {
        assert_eq!(
            data(" DATA:text/css,p%7Bcolor:red%7D"),
            Some((true, None, b"p{color:red}".to_vec()))
        );
        assert_eq!(
            data("data:Text/CSS;charset=windows-1252;base64,cHt9 wqk="),
            Some((true, Some("windows-1252".into()), b"p{}\xc2\xa9".to_vec()))
        );
        assert_eq!(data("data:,p{}"), Some((false, None, b"p{}".to_vec())));
        assert_eq!(data("data:text/css;base64,cHt9="), None, "bad padding");
        assert_eq!(data("data:text/css;base64,c"), None);
        assert_eq!(data("data:text/css"), None, "no comma");
        assert!(data_url("https://example.org/", &Budget::new(1024).unwrap()).is_none());
    }

    fn text(body: &[u8], label: Option<&str>, environment: Encoding) -> Option<(String, Encoding)> {
        let memory = Budget::new(4 << 20).unwrap();
        css_text(body, label.map(str::as_bytes), environment, &memory)
            .unwrap()
            .map(|(text, encoding)| (text.as_str().to_string(), encoding))
    }

    #[test]
    fn a_sheet_is_decoded_by_mark_label_charset_rule_then_environment() {
        let utf8 = Encoding::Utf8;
        let latin = Encoding::Windows1252;
        assert_eq!(
            text(b"\xef\xbb\xbfp{}", Some("windows-1252"), latin),
            Some(("p{}".into(), utf8))
        );
        assert_eq!(
            text(b"a\xe9", Some("windows-1252"), utf8),
            Some(("a\u{e9}".into(), latin))
        );
        assert_eq!(
            text(b"@charset \"windows-1252\";\xe9", Some("bogus"), utf8),
            Some(("@charset \"windows-1252\";\u{e9}".into(), latin))
        );
        assert_eq!(
            text(b"@charset \"utf-16le\";a", None, latin),
            Some(("@charset \"utf-16le\";a".into(), utf8))
        );
        assert_eq!(text(b"\xe9", None, latin), Some(("\u{e9}".into(), latin)));
        assert_eq!(text(b"\xe9", None, utf8), Some(("\u{fffd}".into(), utf8)));
        // Past the bound once decoded: a byte of windows-1252 can take
        // two of UTF-8.
        let wide = vec![0xe9u8; MAX_SHEET / 2 + 1];
        assert_eq!(text(&wide, Some("windows-1252"), utf8), None);
        assert!(text(&vec![b'a'; MAX_SHEET + 1], None, utf8).is_none());
    }

    fn page(markup: &str, url: &str) -> (Tree, Sheets, Budget) {
        let budget = Budget::new(64 << 20).unwrap();
        let tree = Tree::parse(markup, Source::Markup, &budget).unwrap();
        let url = Url::parse(url).unwrap();
        let sheets = Sheets::new(&tree, &url, Encoding::Utf8, &budget).unwrap();
        (tree, sheets, budget)
    }

    fn order(tree: &Tree, sheets: &Sheets) -> Vec<(String, Vec<String>)> {
        let mut out = Vec::new();
        sheets
            .each(tree, |given| {
                out.push((
                    given.text.to_string(),
                    given.lists().iter().map(|l| l.to_string()).collect(),
                ));
                Ok::<_, ()>(())
            })
            .unwrap();
        out
    }

    /// Sheet `url`, wanted, answered with `body`.
    fn reply(sheets: &mut Sheets, url: &str, body: &str) {
        let at = sheets
            .entries
            .iter()
            .position(|entry| {
                matches!(entry.state, State::Wanted)
                    && entry.url.as_ref().map(BudgetString::as_str) == Some(url)
            })
            .unwrap_or_else(|| panic!("{url} is not wanted"));
        let wanted = Url::parse(url).unwrap();
        sheets.asked(at);
        sheets
            .arrived(at, &wanted, Some("text/css"), body.as_bytes())
            .unwrap();
    }

    #[test]
    fn imports_stand_at_their_rules_place_under_their_chains_media() {
        let (tree, mut sheets, _budget) = page(
            "<base href=/css/><style media=screen>@import 'a.css' print; s{}</style>\
             <link rel=stylesheet href=b.css media=all>\
             <link rel=stylesheet href='data:text/css,@import \"https://example.org/d.css\";d{}'>",
            "https://example.org/page",
        );
        reply(
            &mut sheets,
            "https://example.org/css/a.css",
            "@import 'c.css' (x); a{}",
        );
        reply(&mut sheets, "https://example.org/css/b.css", "b{}");
        reply(&mut sheets, "https://example.org/d.css", "dd{}");
        reply(&mut sheets, "https://example.org/css/c.css", "c{}");
        assert!(!sheets.unsettled());
        sheets.finish();
        let named = |text: &str, lists: &[&str]| {
            (
                text.to_string(),
                lists.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            order(&tree, &sheets),
            [
                named("c{}", &["screen", "print", "(x)"]),
                named("@import 'c.css' (x); a{}", &["screen", "print"]),
                named("@import 'a.css' print; s{}", &["screen"]),
                named("b{}", &["all"]),
                named("dd{}", &[]),
                named("@import \"https://example.org/d.css\";d{}", &[]),
            ]
        );
        assert_eq!(sheets.tally(), Tally::default());
    }

    #[test]
    fn imports_td_css_does_not_apply_are_counted_left_out() {
        let unapplied = "@import 'l.css' layer; @import url(s.css) supports(display:grid);";
        let skipped = |skipped| Tally {
            skipped,
            ..Tally::default()
        };
        // In a `<style>` importing nothing else: read from the tree.
        let markup = format!("<style>{unapplied} p{{}}</style>");
        let (tree, sheets, _budget) = page(&markup, "https://example.org/");
        assert!(Sheets::needed(&tree));
        assert!(!sheets.unsettled());
        assert_eq!(sheets.tally(), skipped(2));
        // In a sheet that came, beside an import that is fetched.
        let (_tree, mut sheets, _budget) =
            page("<link rel=stylesheet href=/a.css>", "https://example.org/");
        reply(
            &mut sheets,
            "https://example.org/a.css",
            &format!("{unapplied} @import 'b.css'; @import 3;"),
        );
        reply(&mut sheets, "https://example.org/b.css", "b{}");
        assert!(!sheets.unsettled());
        // The malformed one is an invalid rule, no sheet.
        assert_eq!(sheets.tally(), skipped(2));
    }

    #[test]
    fn imports_nest_four_deep_and_a_cycle_is_left_out() {
        let (tree, mut sheets, _budget) = page(
            "<link rel=stylesheet href=/0.css><link rel=stylesheet href=/x.css>",
            "https://example.org/",
        );
        for n in 0..5 {
            let url = format!("https://example.org/{n}.css");
            reply(
                &mut sheets,
                &url,
                &format!("@import '/{}.css'; s{n}{{}}", n + 1),
            );
        }
        // The fifth import, at depth five, is left out.
        reply(
            &mut sheets,
            "https://example.org/x.css",
            "@import '/x.css'; x{}",
        );
        assert!(!sheets.unsettled());
        sheets.finish();
        assert_eq!(order(&tree, &sheets).len(), 6);
        assert_eq!(
            sheets.tally(),
            Tally {
                skipped: 2,
                ..Tally::default()
            }
        );
    }

    #[test]
    fn past_the_count_mixed_content_and_other_schemes_are_refused() {
        let mut markup = String::new();
        for n in 0..MAX_SHEET_FETCHES + 2 {
            markup.push_str(&format!("<link rel=stylesheet href=/{n}.css>"));
        }
        let (_, sheets, _budget) = page(&markup, "https://example.org/");
        assert_eq!(sheets.tally().skipped, 2);
        let (_, sheets, _budget) = page(
            "<link rel=stylesheet href=http://example.org/a.css>\
             <link rel=stylesheet href=javascript:x>\
             <link rel=stylesheet href='data:text/plain,p{}'>\
             <link rel=stylesheet href='data:text/css;base64,%'>\
             <link rel=stylesheet href=//other.example/b.css>",
            "https://example.org/",
        );
        assert_eq!(
            sheets.tally(),
            Tally {
                failed: 4,
                skipped: 0,
                late: 0
            }
        );
        assert_eq!(
            sheets.wanted().map(|(_, url)| url),
            Some("https://other.example/b.css")
        );
        // Under an http page, an http sheet is no mixed content.
        let (_, sheets, _budget) = page(
            "<link rel=stylesheet href=http://example.org/a.css>",
            "http://example.org/",
        );
        assert!(sheets.wanted().is_some());
    }

    #[test]
    fn entries_stay_within_their_reservation_however_many_sheets_a_page_names() {
        // Style elements that import, data: links that import, and links,
        // a hundred of each: the count stops them all at its bound.
        let mut markup = String::new();
        for n in 0..100 {
            markup.push_str(&format!(
                "<style>@import 's{n}.css'; @import 't{n}.css'; s{n}{{}}</style>\
                 <link rel=stylesheet href='data:text/css,@import \"https://example.org/d{n}.css\";'>\
                 <link rel=stylesheet href=/l{n}.css><style>plain{n}{{}}</style>"
            ));
        }
        let (tree, mut sheets, _budget) = page(&markup, "https://example.org/");
        assert!(sheets.entries.len() <= MAX_ENTRIES);
        assert_eq!(sheets.entries.capacity(), MAX_ENTRIES, "never grown");
        assert_eq!(sheets.imports.capacity(), MAX_SHEET_FETCHES);
        assert_eq!(sheets.taken, MAX_SHEET_FETCHES);
        // Five sheets a style element, its two imports, a data: link, its
        // import and a link: the count is met in the seventh, its data:
        // link and link left out; past it each is counted but the data:
        // link's import, never read.
        assert_eq!(sheets.tally().skipped, 2 + 93 * 4);
        assert!(sheets.bytes() <= MAX_HELD);
        sheets.finish();
        // Every style element is still applied, in tree order, those past
        // the count from the tree.
        let order = order(&tree, &sheets);
        let plain = order
            .iter()
            .filter(|(text, _)| text.starts_with("plain"))
            .count();
        assert_eq!(plain, 100);
        let imported = order
            .iter()
            .filter(|(text, _)| text.starts_with("@import 's"))
            .count();
        assert_eq!(imported, 100);
    }

    #[test]
    fn a_sheet_not_come_is_late_and_one_refused_is_counted() {
        let (tree, mut sheets, _budget) = page(
            "<link rel=stylesheet href=a.css><link rel=stylesheet href=b.css><style>s{}</style>",
            "https://example.org/",
        );
        let (at, _) = sheets.wanted().unwrap();
        sheets.asked(at);
        sheets.refused(at, Refused::Failed);
        sheets.finish();
        assert_eq!(order(&tree, &sheets), [("s{}".to_string(), Vec::new())]);
        assert_eq!(
            sheets.tally(),
            Tally {
                failed: 1,
                skipped: 0,
                late: 1
            }
        );
    }

    #[test]
    fn an_imported_sheet_defaults_to_its_importers_encoding() {
        let (tree, mut sheets, _budget) =
            page("<link rel=stylesheet href=a.css>", "https://example.org/");
        let (at, url) = sheets.wanted().unwrap();
        let url = Url::parse(url).unwrap();
        sheets.asked(at);
        sheets
            .arrived(
                at,
                &url,
                Some("text/css; charset=windows-1252"),
                b"@import 'b.css';",
            )
            .unwrap();
        let (at, url) = sheets.wanted().unwrap();
        let url = Url::parse(url).unwrap();
        sheets
            .arrived(at, &url, Some("text/css"), b"b{content:'\xe9'}")
            .unwrap();
        sheets.finish();
        assert_eq!(order(&tree, &sheets)[0].0, "b{content:'\u{e9}'}");
        assert!(sheets.bytes() > 0);
    }

    /// Sheet `asked`, wanted, answered from `answered` as `body` of
    /// `kind`.
    fn answer(sheets: &mut Sheets, asked: &str, answered: &str, kind: Option<&str>, body: &[u8]) {
        let at = sheets
            .entries
            .iter()
            .position(|entry| {
                matches!(entry.state, State::Wanted)
                    && entry.url.as_ref().map(BudgetString::as_str) == Some(asked)
            })
            .unwrap_or_else(|| panic!("{asked} is not wanted"));
        sheets.asked(at);
        let answered = Url::parse(answered).unwrap();
        sheets.arrived(at, &answered, kind, body).unwrap();
    }

    #[test]
    fn sibling_imports_keep_their_order_whichever_comes_first() {
        let (tree, mut sheets, _budget) =
            page("<link rel=stylesheet href=a.css>", "https://example.org/");
        answer(
            &mut sheets,
            "https://example.org/a.css",
            "https://example.org/a.css",
            Some("text/css"),
            b"@import 'x.css'; @import 'y.css'; a{}",
        );
        // y comes before x; the cascade takes x, then y, then a.
        for name in ["y", "x"] {
            let url = format!("https://example.org/{name}.css");
            answer(
                &mut sheets,
                &url,
                &url,
                None,
                format!("{name}{{}}").as_bytes(),
            );
        }
        sheets.finish();
        let texts: Vec<_> = order(&tree, &sheets).into_iter().map(|(t, _)| t).collect();
        assert_eq!(
            texts,
            ["x{}", "y{}", "@import 'x.css'; @import 'y.css'; a{}"]
        );
    }

    #[test]
    fn an_import_is_joined_against_the_url_its_sheet_answered_from() {
        let (_, mut sheets, _budget) =
            page("<link rel=stylesheet href=/a.css>", "https://example.org/");
        answer(
            &mut sheets,
            "https://example.org/a.css",
            "https://example.org/css/a.css",
            Some("text/css"),
            b"@import 'b.css';",
        );
        assert_eq!(
            sheets.wanted().map(|(_, url)| url),
            Some("https://example.org/css/b.css")
        );
    }

    #[test]
    fn a_link_defaults_to_the_pages_encoding_and_a_label_beats_a_charset_rule() {
        let budget = Budget::new(64 << 20).unwrap();
        let tree = Tree::parse(
            "<link rel=stylesheet href=a.css><link rel=stylesheet href=b.css>",
            Source::Markup,
            &budget,
        )
        .unwrap();
        let url = Url::parse("https://example.org/").unwrap();
        let mut sheets = Sheets::new(&tree, &url, Encoding::Windows1252, &budget).unwrap();
        // No label, no rule: the page's windows-1252.
        answer(
            &mut sheets,
            "https://example.org/a.css",
            "https://example.org/a.css",
            Some("text/css"),
            b"a{content:'\xe9'}",
        );
        // The content-type's label before the sheet's own rule.
        answer(
            &mut sheets,
            "https://example.org/b.css",
            "https://example.org/b.css",
            Some("text/css; charset=utf-8"),
            b"@charset \"windows-1252\"; b{content:'\xc3\xa9'}",
        );
        sheets.finish();
        let texts: Vec<_> = order(&tree, &sheets).into_iter().map(|(t, _)| t).collect();
        assert_eq!(
            texts,
            [
                "a{content:'\u{e9}'}",
                "@charset \"windows-1252\"; b{content:'\u{e9}'}"
            ]
        );
    }

    #[test]
    fn the_head_ends_at_another_statement_and_a_bad_string_resumes_at_its_newline() {
        assert!(imports("@namespace svg url(x); @import 'a.css';").is_empty());
        assert_eq!(imports("@layer x; @import 'a.css';").len(), 1);
        // The bad string ends at the newline; the statement at the `;`
        // after it, so neither `a` nor anything of it is fetched.
        assert_eq!(
            imports("@import 'a;b\n c; @import 'd.css';"),
            [("d.css".to_string(), String::new())]
        );
    }

    #[test]
    fn a_charset_rule_is_read_exactly_as_css_syntax_has_it() {
        assert_eq!(
            charset_rule(b"@charset \"utf-16le\";"),
            Some(Encoding::Utf8)
        );
        assert_eq!(
            charset_rule(b"@charset \"windows-1252\"; a{}"),
            Some(Encoding::Windows1252)
        );
        // A label holding `;` or `"`, or a rule past the first 1024 bytes.
        assert_eq!(charset_rule(b"@charset \"windows-1252;x\";"), None);
        assert_eq!(charset_rule(b"@charset \"a\"b\";"), None);
        let mut far = b"@charset \"".to_vec();
        far.extend(std::iter::repeat_n(b'x', 1020));
        far.extend(b"\";");
        assert_eq!(charset_rule(&far), None);
        assert_eq!(charset_rule(b"@charset 'utf-8';"), None);
    }

    #[test]
    fn a_data_urls_fragment_is_left_out_and_a_page_naming_no_sheet_needs_none() {
        assert_eq!(data("data:text/css,a{}#frag").unwrap().2, b"a{}");
        let tree = |markup: &str| {
            let budget = Budget::new(1 << 20).unwrap();
            Tree::parse(markup, Source::Markup, &budget).unwrap()
        };
        assert!(!Sheets::needed(&tree("<style>a{}</style><p>x")));
        assert!(Sheets::needed(&tree("<style>@import 'a.css';</style>")));
        assert!(Sheets::needed(&tree("<link rel=stylesheet href=a.css>")));
    }
}
