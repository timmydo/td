//! What a controller shows of a stream listing, and how it groups it: the
//! characters a name may show, and the application a stream belongs to.
//! td-audio's CLI, and later the td-mixer panel, name an application alike
//! because each takes it from here, and the daemon filters what it hands a
//! controller with the same `shown`.

use crate::client::{level, SinkInput};
use std::borrow::Borrow;
use std::fmt;

/// Whether a character may reach a controller's display. Control characters
/// (C0, DEL, C1) would let a name drive a terminal; the bidirectional
/// overrides, embeddings and isolates would let it reorder its own row and
/// its neighbours'; zero-width characters and the line and paragraph
/// separators would hide or break it.
pub fn shown(character: char) -> bool {
    !character.is_control()
        && !matches!(
            character,
            '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// `text` as one token: what `shown` refuses dropped, and whitespace made
/// `_`, so a line of tokens splits on spaces.
pub fn token_chars(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars()
        .filter(|character| shown(*character))
        .map(|character| {
            if character.is_whitespace() {
                '_'
            } else {
                character
            }
        })
}

/// What an application without a name of its own is called, before its
/// client's index.
pub const CLIENT_PREFIX: &str = "client-";

/// The application a stream is grouped and addressed under: its
/// `application.name` as a token, which td's daemon sets from the uid for a
/// confined application, or `client-N` for a stream whose client never
/// named itself, or named itself `client-` anything, which would pass for
/// another client's. Built by `of`, which holds a `Named` to a name whose
/// token is not empty and does not begin `client-`.
#[derive(Clone, Copy, Debug)]
pub enum App<'a> {
    Named(&'a str),
    Client(u32),
}

impl<'a> App<'a> {
    pub fn of(stream: &'a SinkInput) -> Self {
        match stream.property("application.name") {
            Some(name)
                if token_chars(name).next().is_some()
                    && !token_chars(name)
                        .take(CLIENT_PREFIX.len())
                        .eq(CLIENT_PREFIX.chars()) =>
            {
                Self::Named(name)
            }
            _ => Self::Client(stream.client),
        }
    }

    /// Whether `text`, as typed, names this application.
    pub fn is(self, text: &str) -> bool {
        match self {
            Self::Named(name) => token_chars(name).eq(text.chars()),
            Self::Client(index) => text.strip_prefix(CLIENT_PREFIX).is_some_and(|digits| {
                let decimal = digits.bytes().all(|byte| byte.is_ascii_digit())
                    && (digits == "0" || !digits.starts_with('0'));
                decimal && digits.parse::<u32>().ok() == Some(index)
            }),
        }
    }

    pub fn same(self, other: App<'_>) -> bool {
        match (self, other) {
            (Self::Named(one), App::Named(two)) => token_chars(one).eq(token_chars(two)),
            (Self::Client(one), App::Client(two)) => one == two,
            _ => false,
        }
    }
}

/// The application's name as it is typed: its token, or `client-N`.
impl fmt::Display for App<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Named(name) => {
                token_chars(name).try_for_each(|character| fmt::Write::write_char(f, character))
            }
            Self::Client(index) => write!(f, "{CLIENT_PREFIX}{index}"),
        }
    }
}

/// An application's level is its loudest stream's, and it is muted when
/// every stream is: on td's daemon a confined application's streams all
/// show the one level the human set for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    pub volume: u32,
    pub muted: bool,
    pub streams: usize,
}

/// The streams of `streams` that `belongs` takes, summed up.
pub fn summary<S: Borrow<SinkInput>>(
    streams: impl IntoIterator<Item = S>,
    belongs: impl Fn(&SinkInput) -> bool,
) -> Summary {
    let mut summary = Summary {
        volume: 0,
        muted: true,
        streams: 0,
    };
    for stream in streams.into_iter() {
        let stream = stream.borrow();
        if !belongs(stream) {
            continue;
        }
        summary.volume = summary.volume.max(level(&stream.volume));
        summary.muted &= stream.muted;
        summary.streams += 1;
    }
    summary
}

/// A stream's title: its media name, else its own name.
pub fn title(stream: &SinkInput) -> &str {
    stream
        .property("media.name")
        .or(stream.name.as_deref())
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag;

    fn stream(index: u32, client: u32, application: Option<&str>) -> SinkInput {
        SinkInput {
            index,
            name: Some("a stream".into()),
            client,
            sink: 0,
            volume: vec![0x10000; 2],
            muted: false,
            properties: application
                .map(|name| vec![tag::text_property("application.name", name)])
                .unwrap_or_default(),
        }
    }

    /// A client cannot pass for another: a name in the `client-N` form is
    /// that client's own `client-N`, and only the canonical decimal names
    /// a client.
    #[test]
    fn an_app_name_cannot_pass_for_another_client() {
        let impostor = stream(1, 7, Some("client-4"));
        assert_eq!(App::of(&impostor).to_string(), "client-7");
        assert!(!App::of(&impostor).is("client-4"));
        let unnamed = stream(2, 4, None);
        assert!(App::of(&unnamed).is("client-4"));
        for refused in ["client-04", "client-+4", "client-4x", "client-", "4"] {
            assert!(!App::of(&unnamed).is(refused), "{refused}");
        }
        assert!(App::of(&stream(3, 0, None)).is("client-0"));
        assert!(App::of(&stream(4, 9, Some("\u{1b}"))).is("client-9"));
    }

    /// A name is matched and shown as its token, and nothing a display
    /// would act on survives.
    #[test]
    fn a_name_is_its_token_and_shows_inert() {
        assert!(App::Named("Pacat Test").is("Pacat_Test"));
        assert!(!App::Named("Pacat Test").is("Pacat Test"));
        assert!(App::Named("a\u{202e}b").same(App::Named("ab")));
        assert!(!App::Named("a").same(App::Client(1)));
        assert_eq!(App::Named("Pacat Test\u{1b}").to_string(), "Pacat_Test");
        // Both ends of every refused range, and each range's neighbours.
        for refused in [
            '\u{0}', '\u{1b}', '\u{1f}', '\u{7f}', '\u{85}', '\u{9f}', '\u{200b}', '\u{200f}',
            '\u{2028}', '\u{2029}', '\u{202a}', '\u{202e}', '\u{2060}', '\u{2066}', '\u{2069}',
            '\u{feff}',
        ] {
            assert!(!shown(refused), "{refused:?}");
        }
        for kept in [
            '\u{a0}', '\u{200a}', '\u{2010}', '\u{2027}', '\u{202f}', '\u{205f}', '\u{206a}',
            '\u{fefe}',
        ] {
            assert!(shown(kept), "{kept:?}");
        }
        assert!(shown('é') && shown(' '));
    }

    /// An application's level is its loudest stream's, muted only when
    /// every stream is; a stream's title is its media name, else its name.
    #[test]
    fn an_application_sums_its_streams() {
        let mut one = stream(1, 3, Some("firefox"));
        one.volume = vec![0x4000, 0x8000];
        one.muted = true;
        let mut two = stream(2, 3, Some("firefox"));
        two.volume = vec![0x2000; 2];
        let other = stream(3, 5, None);
        let all = [one.clone(), two.clone(), other];
        let firefox = summary(&all, |stream| App::of(stream).is("firefox"));
        assert_eq!(
            firefox,
            Summary {
                volume: 0x8000,
                muted: false,
                streams: 2
            }
        );
        // Muted only when every stream is; none is muted and silent.
        two.muted = true;
        assert!(summary([&one, &two], |_| true).muted);
        assert_eq!(
            summary(&all, |stream| App::of(stream).is("nobody")),
            Summary {
                volume: 0,
                muted: true,
                streams: 0
            }
        );
        one.properties
            .push(tag::text_property("media.name", "a video"));
        assert_eq!(title(&one), "a video");
        assert_eq!(title(&two), "a stream");
    }
}
