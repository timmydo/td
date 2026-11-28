//! An `img`'s sources (td-web/DESIGN.md §10, "Subresources"): HTML's
//! "parse a srcset attribute", read lazily with no allocation, and the
//! one candidate chosen for an environment. Parsing and choosing only:
//! the caller fetches, and td-mail, which takes only `src`, never calls
//! it.

use crate::tree::Tree;
use td_alloc::Budget;
use td_css::conditions;
use td_html::document::NodeId;

/// The environment a candidate is chosen for, as `sheets::environment`
/// makes it: named here so a caller need not depend on td-css.
pub use td_css::conditions::Environment;

/// The most bytes of a `srcset` read; a longer one counts as absent.
pub const MAX_SRCSET_BYTES: usize = 64 * 1024;
/// The most candidates of a `srcset` considered; the rest are ignored.
pub const MAX_CANDIDATES: usize = 64;
/// The greatest `x` descriptor taken; a greater one drops its candidate.
pub const MAX_DENSITY: f64 = 1000.0;

/// A candidate's descriptor: a pixel density, or a width in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Descriptor {
    Density(f64),
    Width(u32),
}

/// One image candidate of a `srcset`: its URL as written, at byte `at`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate<'a> {
    pub url: &'a str,
    pub at: usize,
    pub descriptor: Descriptor,
}

/// The candidates of a `srcset`, in order, those in error left out.
pub struct Candidates<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Candidates<'a> {
    pub fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }
}

/// HTML's ASCII white space: tab, line feed, form feed, return, space.
fn space(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | b'\x0c' | b'\r' | b' ')
}

#[derive(Clone, Copy)]
enum State {
    In,
    Parens,
    After,
}

/// HTML's descriptor parser, fed one descriptor at a time.
#[derive(Default)]
struct Parsed {
    width: Option<u32>,
    density: Option<f64>,
    height: Option<u32>,
    error: bool,
}

impl Parsed {
    fn feed(&mut self, descriptor: &str) {
        let (Some(&last), Some(number)) = (
            descriptor.as_bytes().last(),
            descriptor.get(..descriptor.len().saturating_sub(1)),
        ) else {
            self.error = true;
            return;
        };
        match last {
            b'w' if self.width.is_none() && self.density.is_none() => {
                match integer(number).filter(|&n| n > 0) {
                    Some(n) => self.width = Some(n),
                    None => self.error = true,
                }
            }
            b'x' if self.width.is_none() && self.density.is_none() && self.height.is_none() => {
                match float(number).filter(|n| (0.0..=MAX_DENSITY).contains(n)) {
                    // -0x is no less than zero: a density of 0.
                    Some(n) => self.density = Some(n + 0.0),
                    None => self.error = true,
                }
            }
            b'h' if self.height.is_none() && self.density.is_none() => {
                match integer(number).filter(|&n| n > 0) {
                    Some(n) => self.height = Some(n),
                    None => self.error = true,
                }
            }
            _ => self.error = true,
        }
    }

    fn descriptor(&self) -> Option<Descriptor> {
        if self.error || (self.height.is_some() && self.width.is_none()) {
            return None;
        }
        Some(match (self.width, self.density) {
            (Some(width), _) => Descriptor::Width(width),
            (None, Some(density)) => Descriptor::Density(density),
            (None, None) => Descriptor::Density(1.0),
        })
    }
}

/// A valid non-negative integer: ASCII digits alone, within u32.
fn integer(text: &str) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    text.bytes().try_fold(0u32, |n, b| {
        b.is_ascii_digit()
            .then(|| n.checked_mul(10)?.checked_add(u32::from(b - b'0')))
            .flatten()
    })
}

/// A valid floating-point number: `-`?, digits with an optional
/// fraction or a fraction alone, then an optional exponent.
fn float(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut at = usize::from(bytes.first() == Some(&b'-'));
    let digits = |at: &mut usize| {
        let start = *at;
        while bytes.get(*at).is_some_and(u8::is_ascii_digit) {
            *at += 1;
        }
        *at > start
    };
    let whole = digits(&mut at);
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        if !digits(&mut at) {
            return None;
        }
    } else if !whole {
        return None;
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        if matches!(bytes.get(at), Some(b'-' | b'+')) {
            at += 1;
        }
        if !digits(&mut at) {
            return None;
        }
    }
    (at == bytes.len())
        .then(|| text.parse::<f64>().ok())
        .flatten()
        .filter(|n| n.is_finite())
}

impl<'a> Iterator for Candidates<'a> {
    type Item = Candidate<'a>;

    fn next(&mut self) -> Option<Candidate<'a>> {
        let bytes = self.input.as_bytes();
        loop {
            while bytes.get(self.pos).is_some_and(|&b| space(b) || b == b',') {
                self.pos += 1;
            }
            if self.pos >= bytes.len() {
                return None;
            }
            let start = self.pos;
            while bytes.get(self.pos).is_some_and(|&b| !space(b)) {
                self.pos += 1;
            }
            let mut end = self.pos;
            let mut parsed = Parsed::default();
            if end.checked_sub(1).and_then(|i| bytes.get(i)) == Some(&b',') {
                // Trailing commas end the candidate, with no descriptors.
                while end > start && bytes.get(end - 1) == Some(&b',') {
                    end -= 1;
                }
            } else {
                self.descriptors(&mut parsed);
            }
            // Split at ASCII bytes alone, so on scalar boundaries.
            let (Some(url), Some(descriptor)) = (self.input.get(start..end), parsed.descriptor())
            else {
                continue;
            };
            return Some(Candidate {
                url,
                at: start,
                descriptor,
            });
        }
    }
}

impl Candidates<'_> {
    /// HTML's descriptor tokenizer from `pos`, past the candidate's comma
    /// if it has one. Each descriptor is a run of the input: parentheses
    /// keep white space and commas within it.
    fn descriptors(&mut self, parsed: &mut Parsed) {
        let bytes = self.input.as_bytes();
        while bytes.get(self.pos).is_some_and(|&b| space(b)) {
            self.pos += 1;
        }
        let mut state = State::In;
        let mut current: Option<usize> = None;
        let mut feed = |from: Option<usize>, to: usize| {
            if let Some(text) = from.and_then(|from| self.input.get(from..to)) {
                parsed.feed(text);
            }
        };
        loop {
            let c = bytes.get(self.pos).copied();
            match state {
                State::In => match c {
                    Some(b) if space(b) => {
                        feed(current.take(), self.pos);
                        state = State::After;
                    }
                    Some(b',') => {
                        feed(current.take(), self.pos);
                        self.pos += 1;
                        return;
                    }
                    Some(b'(') => {
                        current.get_or_insert(self.pos);
                        state = State::Parens;
                    }
                    None => {
                        feed(current.take(), self.pos);
                        return;
                    }
                    Some(_) => {
                        current.get_or_insert(self.pos);
                    }
                },
                State::Parens => match c {
                    Some(b')') => state = State::In,
                    None => {
                        feed(current.take(), self.pos);
                        return;
                    }
                    Some(_) => {}
                },
                State::After => match c {
                    Some(b) if space(b) => {}
                    None => return,
                    Some(_) => {
                        state = State::In;
                        continue;
                    }
                },
            }
            self.pos += 1;
        }
    }
}

/// Which attribute a chosen URL is in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Attribute {
    Src,
    Srcset,
}

/// The candidate chosen for an image: where its URL is in the tree, and
/// its density, image pixels to a CSS px. Read with `Tree::candidate`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chosen {
    pub holder: NodeId,
    pub attribute: Attribute,
    /// The URL's bytes in a `srcset`; unread for `src`.
    pub span: (u32, u32),
    pub density: f64,
}

impl Chosen {
    /// An image's `src`, at 1x.
    pub fn src(holder: NodeId) -> Self {
        Self {
            holder,
            attribute: Attribute::Src,
            span: (0, 0),
            density: 1.0,
        }
    }
}

/// The candidate `img` shows in `env`: HTML's source set of its `srcset`
/// and its `src`, as "update the source set" and "select an image
/// source" make it, among the candidates `fetchable` takes. A `w`
/// candidate's density is its width over `sizes` (td-css's
/// `source_size`, charged to `memory`; absent or refused, 100vw). `src`
/// is the source set's last, at 1x, unless a candidate is 1x or has a
/// width. Chosen: the least density at least the environment's
/// resolution, else the greatest; of equal densities, the first. With no
/// `srcset`, its `src`, whatever `fetchable` says; none without either.
pub fn select(
    tree: &Tree,
    img: NodeId,
    env: &Environment,
    memory: &Budget,
    fetchable: impl Fn(&str) -> bool,
) -> Option<Chosen> {
    let src = tree.src(img).filter(|src| !src.is_empty());
    let Some(srcset) = tree.srcset(img) else {
        return src.map(|_| Chosen::src(img));
    };
    let candidates = || {
        Candidates::new(srcset)
            .take(MAX_CANDIDATES)
            .filter(|candidate| fetchable(candidate.url))
    };
    let (mut widths, mut one) = (false, false);
    for candidate in candidates() {
        match candidate.descriptor {
            Descriptor::Width(_) => widths = true,
            Descriptor::Density(density) => one |= density == 1.0,
        }
    }
    let size = if widths {
        tree.sizes(img)
            .and_then(|sizes| conditions::source_size(sizes, env, memory).ok())
            .unwrap_or(env.width)
    } else {
        env.width
    };
    let mut least: Option<Chosen> = None;
    let mut most: Option<Chosen> = None;
    let mut consider = |chosen: Chosen| {
        let density = chosen.density;
        if density >= env.resolution_dppx && least.is_none_or(|l| density < l.density) {
            least = Some(chosen);
        }
        if most.is_none_or(|m| density > m.density) {
            most = Some(chosen);
        }
    };
    for candidate in candidates() {
        let density = match candidate.descriptor {
            Descriptor::Density(density) => density,
            // A size of 0 makes it infinitely dense: 0x0 CSS px.
            Descriptor::Width(width) => f64::from(width) / size,
        };
        let start = u32::try_from(candidate.at).unwrap_or(u32::MAX);
        let end = u32::try_from(candidate.at.saturating_add(candidate.url.len()));
        consider(Chosen {
            holder: img,
            attribute: Attribute::Srcset,
            span: (start, end.unwrap_or(u32::MAX)),
            density,
        });
    }
    if src.is_some_and(|src| !widths && !one && fetchable(src)) {
        consider(Chosen::src(img));
    }
    least.or(most)
}

#[cfg(test)]
mod tests;
