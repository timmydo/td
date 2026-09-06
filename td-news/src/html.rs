//! HTML to text rendering.
//!
//! A dependency-free renderer for the subset of HTML that turns up in mail
//! and feed bodies. It replaces the `html2text` crate for td's terminal
//! applications and follows that crate's output conventions (markdown-ish
//! inline markers and `[n]` link footnotes for plain output, tagged spans for
//! rich output) so a reader sees the same page.
//!
//! Input is untrusted: the tokenizer never fails, element nesting is bounded,
//! and every pass is linear in the input size.
//!
//! [`to_text`] renders the plain form (the equivalent of `from_read`) and
//! [`to_rich`] the tagged form (the equivalent of `from_read_coloured`'s input
//! lines). Both run the same parse, measure and layout; they differ only in
//! the decoration a renderer applies, exactly as html2text's `PlainDecorator`
//! and `RichDecorator` do.

/// An RGB colour carried by [`Tag::Colour`] and [`Tag::BgColour`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Inline markup attached to a [`Span`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    Strong,
    Emphasis,
    Strikeout,
    Code,
    Preformat,
    Link(String),
    Image(String),
    Colour(Rgb),
    BgColour(Rgb),
}

/// A run of text sharing one set of tags.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub tags: Vec<Tag>,
}

impl Span {
    fn plain(text: String) -> Span {
        Span {
            text,
            tags: Vec::new(),
        }
    }
}

/// Render `html` as plain text wrapped at `width` columns (0 is unbounded).
///
/// Emphasis is marked up like markdown (`*em*`, `**strong**`, `` `code` ``),
/// links render as `[text][n]` with an `[n]: url` reference list appended.
pub fn to_text(html: &[u8], width: usize) -> String {
    let lines = render(html, width, Decor::Plain);
    let mut out = String::new();
    for line in &lines {
        for span in line {
            out.push_str(&span.text);
        }
        out.push('\n');
    }
    out
}

/// Render `html` as lines of tagged spans, wrapped at `width` columns.
///
/// Markup is carried by [`Tag`] values rather than by literal characters, so
/// a caller can map it to terminal attributes.
pub fn to_rich(html: &[u8], width: usize) -> Vec<Vec<Span>> {
    render(html, width, Decor::Rich)
}

/// Maximum element nesting kept from the input; deeper elements are
/// transparent (their text still renders).
const MAX_DEPTH: usize = 64;
/// Maximum open elements tracked for end-tag matching. Beyond this a start
/// tag is transparent, which keeps matching bounded on a flood of unclosed
/// tags.
const MAX_OPEN: usize = 256;
/// Maximum nesting of laid-out tables; deeper tables render as plain blocks.
const MAX_TABLE_DEPTH: usize = 4;
/// Maximum columns in a laid-out table.
const MAX_COLS: usize = 64;
/// Tab stop used inside `<pre>`.
const TAB_STOP: usize = 8;
/// Combining long stroke overlay, used to strike text out.
const STRIKE: char = '\u{336}';

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decor {
    /// html2text's `PlainDecorator` plus `do_decorate()` and link footnotes.
    Plain,
    /// html2text's `RichDecorator`: tags only, no literal markers.
    Rich,
}

fn render(html: &[u8], width: usize, decor: Decor) -> Vec<Vec<Span>> {
    let source = String::from_utf8_lossy(html);
    let nodes = build(&source);
    let (est, content) = measure(&nodes, decor);
    let mut rend = Rend {
        nodes: &nodes,
        est,
        content,
        decor,
        tags: Vec::new(),
        links: Vec::new(),
        strike: 0,
        pre: 0,
        table_depth: 0,
    };
    let mut out = Out::new(width);
    rend.children(0, &mut out);
    if decor == Decor::Plain && !rend.links.is_empty() {
        out.start_block();
        let links = std::mem::take(&mut rend.links);
        for (i, url) in links.iter().enumerate() {
            push_footnote(&mut out, i + 1, url);
        }
    }
    let mut lines = out.into_lines();
    while lines.last().is_some_and(|l| !line_has_content(l)) {
        lines.pop();
    }
    lines
}

/// Emit `[n]: url`, hard-wrapping the target rather than breaking after the
/// marker, so the whole reference stays recognisable on its first line.
fn push_footnote(out: &mut Out, index: usize, url: &str) {
    let prefix = format!("[{}]: ", index);
    let width = out.width;
    if width == 0 {
        out.push_line(vec![Span::plain(format!("{prefix}{url}"))]);
        return;
    }
    let mut room = width.saturating_sub(str_width(&prefix));
    let mut line = prefix;
    let mut chunk = String::new();
    for c in url.chars() {
        let cw = char_width(c);
        if cw > room {
            line.push_str(&chunk);
            out.push_line(vec![Span::plain(std::mem::take(&mut line))]);
            chunk.clear();
            room = width;
        }
        chunk.push(c);
        room -= cw.min(room);
    }
    line.push_str(&chunk);
    if !line.is_empty() {
        out.push_line(vec![Span::plain(line)]);
    }
}

fn line_has_content(line: &[Span]) -> bool {
    line.iter().any(|s| !s.text.is_empty())
}

// ---------------------------------------------------------------------------
// Display width
// ---------------------------------------------------------------------------

/// Columns taken by `c`: 0 for combining and zero-width marks, 2 for East
/// Asian wide and fullwidth characters, 1 otherwise.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp < 0x300 {
        // Fast path: Latin, Greek, Cyrillic and punctuation are all 1 wide,
        // apart from the soft hyphen which we render as invisible.
        return if cp == 0xad { 0 } else { 1 };
    }
    if is_zero_width(cp) {
        return 0;
    }
    if is_wide(cp) {
        return 2;
    }
    1
}

fn is_zero_width(cp: u32) -> bool {
    matches!(cp,
        0x300..=0x36f
            | 0x483..=0x489
            | 0x591..=0x5bd
            | 0x5bf
            | 0x5c1..=0x5c2
            | 0x5c4..=0x5c5
            | 0x5c7
            | 0x610..=0x61a
            | 0x64b..=0x65f
            | 0x670
            | 0x6d6..=0x6dc
            | 0x6df..=0x6e4
            | 0x6e7..=0x6e8
            | 0x6ea..=0x6ed
            | 0x711
            | 0x730..=0x74a
            | 0x7a6..=0x7b0
            | 0x816..=0x819
            | 0x81b..=0x823
            | 0x825..=0x827
            | 0x829..=0x82d
            | 0x8d3..=0x8ff
            | 0x93a
            | 0x93c
            | 0x941..=0x948
            | 0x94d
            | 0x951..=0x957
            | 0x962..=0x963
            | 0x981
            | 0x9bc
            | 0x9c1..=0x9c4
            | 0x9cd
            | 0xa01..=0xa02
            | 0xa3c
            | 0xa41..=0xa42
            | 0xa47..=0xa48
            | 0xa4b..=0xa4d
            | 0xb01
            | 0xb3c
            | 0xb3f
            | 0xb41..=0xb44
            | 0xb4d
            | 0xbc0
            | 0xbcd
            | 0xc3e..=0xc40
            | 0xc46..=0xc48
            | 0xc4a..=0xc4d
            | 0xcbc
            | 0xccc..=0xccd
            | 0xd41..=0xd44
            | 0xd4d
            | 0xdca
            | 0xdd2..=0xdd4
            | 0xe31
            | 0xe34..=0xe3a
            | 0xe47..=0xe4e
            | 0xeb1
            | 0xeb4..=0xebc
            | 0xec8..=0xecd
            | 0xf35
            | 0xf37
            | 0xf39
            | 0xf71..=0xf7e
            | 0xf80..=0xf84
            | 0xf86..=0xf87
            | 0x102d..=0x1030
            | 0x1032..=0x1037
            | 0x1039..=0x103a
            | 0x1058..=0x1059
            | 0x135d..=0x135f
            | 0x1712..=0x1714
            | 0x1752..=0x1753
            | 0x17b4..=0x17b5
            | 0x17b7..=0x17bd
            | 0x17c6
            | 0x17c9..=0x17d3
            | 0x180b..=0x180e
            | 0x18a9
            | 0x1a17..=0x1a18
            | 0x1ab0..=0x1aff
            | 0x1b00..=0x1b03
            | 0x1b34
            | 0x1b36..=0x1b3a
            | 0x1b6b..=0x1b73
            | 0x1dc0..=0x1dff
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x206a..=0x206f
            | 0x20d0..=0x20f0
            | 0x2cef..=0x2cf1
            | 0x302a..=0x302d
            | 0x3099..=0x309a
            | 0xa66f..=0xa672
            | 0xa806
            | 0xa80b
            | 0xa825..=0xa826
            | 0xfb1e
            | 0xfe00..=0xfe0f
            | 0xfe20..=0xfe2f
            | 0xfeff
            | 0x101fd
            | 0x1d167..=0x1d169
            | 0x1d17b..=0x1d182
            | 0x1d185..=0x1d18b
            | 0x1d1aa..=0x1d1ad
            | 0xe0100..=0xe01ef
    )
}

fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0x303e
            | 0x3041..=0x33ff
            | 0x3400..=0x4dbf
            | 0x4e00..=0x9fff
            | 0xa000..=0xa4cf
            | 0xa960..=0xa97f
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x16fe0..=0x16fe4
            | 0x17000..=0x18d08
            | 0x1b000..=0x1b16f
            | 0x1f004
            | 0x1f0cf
            | 0x1f18e
            | 0x1f191..=0x1f19a
            | 0x1f200..=0x1f320
            | 0x1f32d..=0x1f335
            | 0x1f337..=0x1f37c
            | 0x1f37e..=0x1f393
            | 0x1f3a0..=0x1f3ca
            | 0x1f3cf..=0x1f3d3
            | 0x1f3e0..=0x1f3f0
            | 0x1f3f4
            | 0x1f3f8..=0x1f43e
            | 0x1f440
            | 0x1f442..=0x1f4fc
            | 0x1f4ff..=0x1f53d
            | 0x1f54b..=0x1f54e
            | 0x1f550..=0x1f567
            | 0x1f57a
            | 0x1f595..=0x1f596
            | 0x1f5a4
            | 0x1f5fb..=0x1f64f
            | 0x1f680..=0x1f6c5
            | 0x1f6cc
            | 0x1f6d0..=0x1f6d2
            | 0x1f6eb..=0x1f6ec
            | 0x1f6f4..=0x1f6fc
            | 0x1f7e0..=0x1f7eb
            | 0x1f90c..=0x1f9ff
            | 0x1fa70..=0x1faff
            | 0x20000..=0x2fffd
            | 0x30000..=0x3fffd
    )
}

fn str_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// True for the characters that collapse into a single space and offer a
/// wrap point. The non-breaking spaces are deliberately excluded: they are
/// content that must not become a wrap point.
fn is_html_ws(c: char) -> bool {
    matches!(c,
        ' ' | '\t' | '\n' | '\r' | '\x0c'
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Control characters which never reach the output.
fn is_ignorable(c: char) -> bool {
    let cp = c as u32;
    (cp < 0x20 && !is_html_ws(c)) || (0x7f..=0x9f).contains(&cp)
}

// ---------------------------------------------------------------------------
// Character entities
// ---------------------------------------------------------------------------

/// Named character references: name, replacement, and whether browsers also
/// decode the name without its trailing semicolon.
static ENTITIES: &[(&str, char, bool)] = &[
    ("AElig", '\u{c6}', true),
    ("AMP", '\u{26}', true),
    ("Aacute", '\u{c1}', true),
    ("Acirc", '\u{c2}', true),
    ("Agrave", '\u{c0}', true),
    ("Alpha", '\u{391}', false),
    ("Aring", '\u{c5}', true),
    ("Atilde", '\u{c3}', true),
    ("Auml", '\u{c4}', true),
    ("Beta", '\u{392}', false),
    ("COPY", '\u{a9}', true),
    ("Ccedil", '\u{c7}', true),
    ("Chi", '\u{3a7}', false),
    ("Dagger", '\u{2021}', false),
    ("Delta", '\u{394}', false),
    ("ETH", '\u{d0}', true),
    ("Eacute", '\u{c9}', true),
    ("Ecirc", '\u{ca}', true),
    ("Egrave", '\u{c8}', true),
    ("Epsilon", '\u{395}', false),
    ("Eta", '\u{397}', false),
    ("Euml", '\u{cb}', true),
    ("GT", '\u{3e}', true),
    ("Gamma", '\u{393}', false),
    ("Iacute", '\u{cd}', true),
    ("Icirc", '\u{ce}', true),
    ("Igrave", '\u{cc}', true),
    ("Iota", '\u{399}', false),
    ("Iuml", '\u{cf}', true),
    ("Kappa", '\u{39a}', false),
    ("LT", '\u{3c}', true),
    ("Lambda", '\u{39b}', false),
    ("Mu", '\u{39c}', false),
    ("NewLine", '\u{a}', false),
    ("Ntilde", '\u{d1}', true),
    ("Nu", '\u{39d}', false),
    ("OElig", '\u{152}', false),
    ("Oacute", '\u{d3}', true),
    ("Ocirc", '\u{d4}', true),
    ("Ograve", '\u{d2}', true),
    ("Omega", '\u{3a9}', false),
    ("Omicron", '\u{39f}', false),
    ("Oslash", '\u{d8}', true),
    ("Otilde", '\u{d5}', true),
    ("Ouml", '\u{d6}', true),
    ("Phi", '\u{3a6}', false),
    ("Pi", '\u{3a0}', false),
    ("Prime", '\u{2033}', false),
    ("Psi", '\u{3a8}', false),
    ("QUOT", '\u{22}', true),
    ("REG", '\u{ae}', true),
    ("Rho", '\u{3a1}', false),
    ("Scaron", '\u{160}', false),
    ("Sigma", '\u{3a3}', false),
    ("THORN", '\u{de}', true),
    ("Tab", '\u{9}', false),
    ("Tau", '\u{3a4}', false),
    ("Theta", '\u{398}', false),
    ("Uacute", '\u{da}', true),
    ("Ucirc", '\u{db}', true),
    ("Ugrave", '\u{d9}', true),
    ("Upsilon", '\u{3a5}', false),
    ("Uuml", '\u{dc}', true),
    ("Xi", '\u{39e}', false),
    ("Yacute", '\u{dd}', true),
    ("Yuml", '\u{178}', false),
    ("Zeta", '\u{396}', false),
    ("aacute", '\u{e1}', true),
    ("acirc", '\u{e2}', true),
    ("acute", '\u{b4}', true),
    ("aelig", '\u{e6}', true),
    ("agrave", '\u{e0}', true),
    ("alefsym", '\u{2135}', false),
    ("alpha", '\u{3b1}', false),
    ("amp", '\u{26}', true),
    ("and", '\u{2227}', false),
    ("ang", '\u{2220}', false),
    ("angst", '\u{c5}', false),
    ("apos", '\u{27}', false),
    ("aring", '\u{e5}', true),
    ("ast", '\u{2a}', false),
    ("asymp", '\u{2248}', false),
    ("atilde", '\u{e3}', true),
    ("auml", '\u{e4}', true),
    ("bdquo", '\u{201e}', false),
    ("beta", '\u{3b2}', false),
    ("bigstar", '\u{2605}', false),
    ("blacksquare", '\u{25aa}', false),
    ("blank", '\u{2423}', false),
    ("boxh", '\u{2500}', false),
    ("boxv", '\u{2502}', false),
    ("brvbar", '\u{a6}', true),
    ("bsol", '\u{5c}', false),
    ("bull", '\u{2022}', false),
    ("cap", '\u{2229}', false),
    ("ccedil", '\u{e7}', true),
    ("cedil", '\u{b8}', true),
    ("cent", '\u{a2}', true),
    ("check", '\u{2713}', false),
    ("chi", '\u{3c7}', false),
    ("circ", '\u{2c6}', false),
    ("circledS", '\u{24c8}', false),
    ("clubs", '\u{2663}', false),
    ("colon", '\u{3a}', false),
    ("comma", '\u{2c}', false),
    ("commat", '\u{40}', false),
    ("cong", '\u{2245}', false),
    ("copy", '\u{a9}', true),
    ("crarr", '\u{21b5}', false),
    ("cross", '\u{2717}', false),
    ("cup", '\u{222a}', false),
    ("curren", '\u{a4}', true),
    ("dArr", '\u{21d3}', false),
    ("dagger", '\u{2020}', false),
    ("darr", '\u{2193}', false),
    ("dash", '\u{2010}', false),
    ("deg", '\u{b0}', true),
    ("delta", '\u{3b4}', false),
    ("diams", '\u{2666}', false),
    ("divide", '\u{f7}', true),
    ("dollar", '\u{24}', false),
    ("downarrow", '\u{2193}', false),
    ("dtri", '\u{25bf}', false),
    ("eacute", '\u{e9}', true),
    ("ecirc", '\u{ea}', true),
    ("egrave", '\u{e8}', true),
    ("empty", '\u{2205}', false),
    ("emsp", '\u{2003}', false),
    ("ensp", '\u{2002}', false),
    ("epsilon", '\u{3b5}', false),
    ("equals", '\u{3d}', false),
    ("equiv", '\u{2261}', false),
    ("eta", '\u{3b7}', false),
    ("eth", '\u{f0}', true),
    ("euml", '\u{eb}', true),
    ("euro", '\u{20ac}', false),
    ("excl", '\u{21}', false),
    ("exist", '\u{2203}', false),
    ("female", '\u{2640}', false),
    ("flat", '\u{266d}', false),
    ("fnof", '\u{192}', false),
    ("forall", '\u{2200}', false),
    ("frac12", '\u{bd}', true),
    ("frac13", '\u{2153}', false),
    ("frac14", '\u{bc}', true),
    ("frac15", '\u{2155}', false),
    ("frac16", '\u{2159}', false),
    ("frac18", '\u{215b}', false),
    ("frac23", '\u{2154}', false),
    ("frac25", '\u{2156}', false),
    ("frac34", '\u{be}', true),
    ("frac35", '\u{2157}', false),
    ("frac38", '\u{215c}', false),
    ("frac45", '\u{2158}', false),
    ("frac56", '\u{215a}', false),
    ("frac58", '\u{215d}', false),
    ("frac78", '\u{215e}', false),
    ("frasl", '\u{2044}', false),
    ("frown", '\u{2322}', false),
    ("gamma", '\u{3b3}', false),
    ("ge", '\u{2265}', false),
    ("grave", '\u{60}', false),
    ("gt", '\u{3e}', true),
    ("hArr", '\u{21d4}', false),
    ("half", '\u{bd}', false),
    ("harr", '\u{2194}', false),
    ("hearts", '\u{2665}', false),
    ("hellip", '\u{2026}', false),
    ("hybull", '\u{2043}', false),
    ("hyphen", '\u{2010}', false),
    ("iacute", '\u{ed}', true),
    ("icirc", '\u{ee}', true),
    ("iexcl", '\u{a1}', true),
    ("igrave", '\u{ec}', true),
    ("image", '\u{2111}', false),
    ("infin", '\u{221e}', false),
    ("int", '\u{222b}', false),
    ("iota", '\u{3b9}', false),
    ("iquest", '\u{bf}', true),
    ("isin", '\u{2208}', false),
    ("iuml", '\u{ef}', true),
    ("kappa", '\u{3ba}', false),
    ("lArr", '\u{21d0}', false),
    ("lambda", '\u{3bb}', false),
    ("lang", '\u{27e8}', false),
    ("laquo", '\u{ab}', true),
    ("larr", '\u{2190}', false),
    ("lceil", '\u{2308}', false),
    ("lcub", '\u{7b}', false),
    ("ldquo", '\u{201c}', false),
    ("le", '\u{2264}', false),
    ("leftarrow", '\u{2190}', false),
    ("leftrightarrow", '\u{2194}', false),
    ("lfloor", '\u{230a}', false),
    ("lowast", '\u{2217}', false),
    ("lowbar", '\u{5f}', false),
    ("loz", '\u{25ca}', false),
    ("lpar", '\u{28}', false),
    ("lrm", '\u{200e}', false),
    ("lsaquo", '\u{2039}', false),
    ("lsqb", '\u{5b}', false),
    ("lsquo", '\u{2018}', false),
    ("lt", '\u{3c}', true),
    ("macr", '\u{af}', true),
    ("male", '\u{2642}', false),
    ("malt", '\u{2720}', false),
    ("mdash", '\u{2014}', false),
    ("micro", '\u{b5}', true),
    ("middot", '\u{b7}', true),
    ("minus", '\u{2212}', false),
    ("mldr", '\u{2026}', false),
    ("mu", '\u{3bc}', false),
    ("nabla", '\u{2207}', false),
    ("natur", '\u{266e}', false),
    ("nbsp", '\u{a0}', true),
    ("ndash", '\u{2013}', false),
    ("ne", '\u{2260}', false),
    ("ni", '\u{220b}', false),
    ("nldr", '\u{2025}', false),
    ("not", '\u{ac}', true),
    ("notin", '\u{2209}', false),
    ("nsub", '\u{2284}', false),
    ("ntilde", '\u{f1}', true),
    ("nu", '\u{3bd}', false),
    ("num", '\u{23}', false),
    ("numero", '\u{2116}', false),
    ("oacute", '\u{f3}', true),
    ("ocirc", '\u{f4}', true),
    ("oelig", '\u{153}', false),
    ("ograve", '\u{f2}', true),
    ("oline", '\u{203e}', false),
    ("omega", '\u{3c9}', false),
    ("omicron", '\u{3bf}', false),
    ("oplus", '\u{2295}', false),
    ("or", '\u{2228}', false),
    ("ordf", '\u{aa}', true),
    ("ordm", '\u{ba}', true),
    ("oslash", '\u{f8}', true),
    ("otilde", '\u{f5}', true),
    ("otimes", '\u{2297}', false),
    ("ouml", '\u{f6}', true),
    ("para", '\u{b6}', true),
    ("part", '\u{2202}', false),
    ("percnt", '\u{25}', false),
    ("period", '\u{2e}', false),
    ("permil", '\u{2030}', false),
    ("perp", '\u{22a5}', false),
    ("phi", '\u{3c6}', false),
    ("phone", '\u{260e}', false),
    ("pi", '\u{3c0}', false),
    ("piv", '\u{3d6}', false),
    ("plus", '\u{2b}', false),
    ("plusmn", '\u{b1}', true),
    ("pound", '\u{a3}', true),
    ("prime", '\u{2032}', false),
    ("prod", '\u{220f}', false),
    ("prop", '\u{221d}', false),
    ("psi", '\u{3c8}', false),
    ("quest", '\u{3f}', false),
    ("quot", '\u{22}', true),
    ("rArr", '\u{21d2}', false),
    ("radic", '\u{221a}', false),
    ("rang", '\u{27e9}', false),
    ("raquo", '\u{bb}', true),
    ("rarr", '\u{2192}', false),
    ("rceil", '\u{2309}', false),
    ("rcub", '\u{7d}', false),
    ("rdquo", '\u{201d}', false),
    ("real", '\u{211c}', false),
    ("reg", '\u{ae}', true),
    ("rfloor", '\u{230b}', false),
    ("rho", '\u{3c1}', false),
    ("rightarrow", '\u{2192}', false),
    ("rlm", '\u{200f}', false),
    ("rpar", '\u{29}', false),
    ("rsaquo", '\u{203a}', false),
    ("rsqb", '\u{5d}', false),
    ("rsquo", '\u{2019}', false),
    ("sbquo", '\u{201a}', false),
    ("scaron", '\u{161}', false),
    ("sdot", '\u{22c5}', false),
    ("sect", '\u{a7}', true),
    ("semi", '\u{3b}', false),
    ("sext", '\u{2736}', false),
    ("sharp", '\u{266f}', false),
    ("shy", '\u{ad}', true),
    ("sigma", '\u{3c3}', false),
    ("sigmaf", '\u{3c2}', false),
    ("sim", '\u{223c}', false),
    ("smile", '\u{2323}', false),
    ("sol", '\u{2f}', false),
    ("spades", '\u{2660}', false),
    ("squ", '\u{25a1}', false),
    ("square", '\u{25a1}', false),
    ("star", '\u{2606}', false),
    ("starf", '\u{2605}', false),
    ("sub", '\u{2282}', false),
    ("sube", '\u{2286}', false),
    ("sum", '\u{2211}', false),
    ("sup", '\u{2283}', false),
    ("sup1", '\u{b9}', true),
    ("sup2", '\u{b2}', true),
    ("sup3", '\u{b3}', true),
    ("supe", '\u{2287}', false),
    ("szlig", '\u{df}', true),
    ("tau", '\u{3c4}', false),
    ("there4", '\u{2234}', false),
    ("theta", '\u{3b8}', false),
    ("thetasym", '\u{3d1}', false),
    ("thinsp", '\u{2009}', false),
    ("thorn", '\u{fe}', true),
    ("tilde", '\u{2dc}', false),
    ("times", '\u{d7}', true),
    ("trade", '\u{2122}', false),
    ("triangle", '\u{25b5}', false),
    ("uArr", '\u{21d1}', false),
    ("uacute", '\u{fa}', true),
    ("uarr", '\u{2191}', false),
    ("ucirc", '\u{fb}', true),
    ("ugrave", '\u{f9}', true),
    ("uml", '\u{a8}', true),
    ("uparrow", '\u{2191}', false),
    ("upsih", '\u{3d2}', false),
    ("upsilon", '\u{3c5}', false),
    ("utri", '\u{25b5}', false),
    ("uuml", '\u{fc}', true),
    ("verbar", '\u{7c}', false),
    ("weierp", '\u{2118}', false),
    ("xi", '\u{3be}', false),
    ("yacute", '\u{fd}', true),
    ("yen", '\u{a5}', true),
    ("yuml", '\u{ff}', true),
    ("zeta", '\u{3b6}', false),
    ("zwj", '\u{200d}', false),
    ("zwnj", '\u{200c}', false),
];

/// Windows-1252 replacements browsers apply to numeric references in the C1
/// range, which real mail does emit (`&#146;` for a right quote).
static C1_MAP: [char; 32] = [
    '\u{20ac}', '\u{81}', '\u{201a}', '\u{192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{2c6}', '\u{2030}', '\u{160}', '\u{2039}', '\u{152}', '\u{8d}', '\u{17d}', '\u{8f}',
    '\u{90}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{2dc}', '\u{2122}', '\u{161}', '\u{203a}', '\u{153}', '\u{9d}', '\u{17e}', '\u{178}',
];

fn lookup_entity(name: &str) -> Option<(char, bool)> {
    ENTITIES
        .binary_search_by(|probe| probe.0.cmp(name))
        .ok()
        .and_then(|i| ENTITIES.get(i))
        .map(|e| (e.1, e.2))
}

fn from_codepoint(cp: u32) -> char {
    if cp == 0 || cp > 0x10_ffff || (0xd800..=0xdfff).contains(&cp) {
        return '\u{fffd}';
    }
    if (0x80..0xa0).contains(&cp) {
        if let Some(c) = C1_MAP.get((cp - 0x80) as usize) {
            return *c;
        }
    }
    char::from_u32(cp).unwrap_or('\u{fffd}')
}

/// Decode character references in `s`. Unknown references are left alone, as
/// browsers do.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes.get(i).copied().unwrap_or(0);
        if b != b'&' {
            // Copy the whole run up to the next '&' in one go.
            let rest = s.get(i..).unwrap_or("");
            let end = rest.find('&').unwrap_or(rest.len());
            out.push_str(rest.get(..end).unwrap_or(""));
            i += end.max(1);
            continue;
        }
        match decode_one(s, i) {
            Some((text, next)) => {
                out.push(text);
                i = next;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// Decode the reference starting at `start` (which indexes a `&`), returning
/// the character and the index just past the reference.
fn decode_one(s: &str, start: usize) -> Option<(char, usize)> {
    let bytes = s.as_bytes();
    let mut i = start + 1;
    if bytes.get(i) == Some(&b'#') {
        i += 1;
        let hex = matches!(bytes.get(i), Some(b'x') | Some(b'X'));
        if hex {
            i += 1;
        }
        let digits_start = i;
        let mut value: u32 = 0;
        while let Some(&d) = bytes.get(i) {
            let v = match (hex, d) {
                (true, b'0'..=b'9') => u32::from(d - b'0'),
                (true, b'a'..=b'f') => u32::from(d - b'a') + 10,
                (true, b'A'..=b'F') => u32::from(d - b'A') + 10,
                (false, b'0'..=b'9') => u32::from(d - b'0'),
                _ => break,
            };
            // Saturate rather than overflow on absurdly long digit runs.
            value = value
                .saturating_mul(if hex { 16 } else { 10 })
                .saturating_add(v);
            value = value.min(0x11_0000);
            i += 1;
        }
        if i == digits_start {
            return None;
        }
        if bytes.get(i) == Some(&b';') {
            i += 1;
        }
        return Some((from_codepoint(value), i));
    }
    // Named reference: at most 32 alphanumeric characters.
    let name_start = i;
    while i < bytes.len() && i - name_start < 32 {
        match bytes.get(i) {
            Some(c) if c.is_ascii_alphanumeric() => i += 1,
            _ => break,
        }
    }
    if i == name_start {
        return None;
    }
    let name = s.get(name_start..i)?;
    if bytes.get(i) == Some(&b';') {
        if let Some((c, _)) = lookup_entity(name) {
            return Some((c, i + 1));
        }
    }
    // Legacy form without a semicolon: longest matching legacy name wins.
    let mut len = name.len();
    while len > 1 {
        if let Some(prefix) = name.get(..len) {
            if let Some((c, legacy)) = lookup_entity(prefix) {
                if legacy {
                    return Some((c, name_start + len));
                }
            }
        }
        len -= 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

struct StartTag {
    name: String,
    attrs: Vec<(String, String)>,
    self_closing: bool,
}

impl StartTag {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

enum Tok {
    Text(String),
    Start(StartTag),
    End(String),
}

struct Lexer<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Lexer<'a> {
        Lexer { src, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        self.src.get(self.pos..).unwrap_or("")
    }

    fn byte(&self, off: usize) -> Option<u8> {
        self.src.as_bytes().get(self.pos + off).copied()
    }

    /// Skip past `needle`, or to the end of the input if it is absent.
    fn skip_past(&mut self, needle: &str) {
        match self.rest().find(needle) {
            Some(at) => self.pos += at + needle.len(),
            None => self.pos = self.src.len(),
        }
    }

    fn next_token(&mut self) -> Option<Tok> {
        loop {
            if self.pos >= self.src.len() {
                return None;
            }
            if self.byte(0) != Some(b'<') {
                let rest = self.rest();
                let end = rest.find('<').unwrap_or(rest.len());
                let text = rest.get(..end).unwrap_or("");
                self.pos += end;
                return Some(Tok::Text(decode_entities(text)));
            }
            match self.byte(1) {
                Some(b'!') => {
                    if self.rest().starts_with("<!--") {
                        self.pos += 4;
                        self.skip_past("-->");
                    } else if self.rest().starts_with("<![CDATA[") {
                        self.pos += 9;
                        self.skip_past("]]>");
                    } else {
                        self.skip_past(">");
                    }
                    continue;
                }
                Some(b'?') => {
                    self.skip_past(">");
                    continue;
                }
                Some(b'/') => {
                    let save = self.pos;
                    self.pos += 2;
                    let name = self.tag_name();
                    self.skip_past(">");
                    if name.is_empty() {
                        // `</>` is text, not a tag.
                        self.pos = save + 1;
                        return Some(Tok::Text("<".to_string()));
                    }
                    return Some(Tok::End(name));
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.pos += 1;
                    return Some(Tok::Start(self.start_tag()));
                }
                _ => {
                    self.pos += 1;
                    return Some(Tok::Text("<".to_string()));
                }
            }
        }
    }

    fn tag_name(&mut self) -> String {
        let rest = self.rest();
        let mut end = 0usize;
        for (i, c) in rest.char_indices() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':' {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        self.pos += end;
        rest.get(..end).unwrap_or("").to_ascii_lowercase()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.byte(0) {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn start_tag(&mut self) -> StartTag {
        let name = self.tag_name();
        let mut attrs = Vec::new();
        let mut self_closing = false;
        loop {
            self.skip_ws();
            match self.byte(0) {
                None => break,
                Some(b'>') => {
                    self.pos += 1;
                    break;
                }
                Some(b'/') => {
                    if self.byte(1) == Some(b'>') {
                        self_closing = true;
                        self.pos += 2;
                        break;
                    }
                    self.pos += 1;
                }
                _ => {
                    if let Some(attr) = self.attribute() {
                        attrs.push(attr);
                    }
                }
            }
        }
        StartTag {
            name,
            attrs,
            self_closing,
        }
    }

    fn attribute(&mut self) -> Option<(String, String)> {
        let rest = self.rest();
        let mut end = 0usize;
        for (i, c) in rest.char_indices() {
            if c.is_whitespace() || c == '=' || c == '>' || c == '/' {
                break;
            }
            end = i + c.len_utf8();
        }
        if end == 0 {
            // Not a name: drop one character so the caller cannot loop.
            let step = rest.chars().next().map_or(1, char::len_utf8);
            self.pos += step;
            return None;
        }
        let name = rest.get(..end).unwrap_or("").to_ascii_lowercase();
        self.pos += end;
        self.skip_ws();
        if self.byte(0) != Some(b'=') {
            return Some((name, String::new()));
        }
        self.pos += 1;
        self.skip_ws();
        let value = match self.byte(0) {
            Some(q @ (b'"' | b'\'')) => {
                self.pos += 1;
                let rest = self.rest();
                let end = rest.find(q as char).unwrap_or(rest.len());
                let value = rest.get(..end).unwrap_or("");
                self.pos += end + usize::from(end < rest.len());
                value
            }
            _ => {
                let rest = self.rest();
                let mut end = rest.len();
                for (i, c) in rest.char_indices() {
                    if c.is_whitespace() || c == '>' {
                        end = i;
                        break;
                    }
                }
                let value = rest.get(..end).unwrap_or("");
                self.pos += end;
                value
            }
        };
        Some((name, decode_entities(value)))
    }

    /// Consume everything up to `</name>` (used for `<script>` and friends).
    fn skip_element(&mut self, name: &str) {
        let close = format!("</{}", name);
        let rest = self.rest();
        let lower = rest.to_ascii_lowercase();
        match lower.find(&close) {
            Some(at) => {
                self.pos += at;
                self.skip_past(">");
            }
            None => self.pos = self.src.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Element tree
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ElemKind {
    /// Inline pass-through (`span`, unknown elements).
    Container,
    Para,
    Div,
    Heading(u8),
    Quote,
    Pre,
    Rule,
    Break,
    Ul,
    Ol,
    Li,
    Dl,
    Dt,
    Dd,
    Table,
    Row,
    Cell,
    Link,
    Image,
    Em,
    Strong,
    Strike,
    Code,
    Sup,
}

struct Elem {
    kind: ElemKind,
    /// Literal marker used in plain output (html2text's `do_decorate` rules).
    mark: &'static str,
    /// `href` for a link, `src` for an image.
    target: String,
    /// `alt` for an image.
    alt: String,
    /// `start` for an ordered list.
    start: i64,
    /// `colspan` for a table cell.
    colspan: usize,
    colour: Option<Rgb>,
    bg: Option<Rgb>,
}

impl Elem {
    fn simple(kind: ElemKind) -> Elem {
        Elem {
            kind,
            mark: "",
            target: String::new(),
            alt: String::new(),
            start: 1,
            colspan: 1,
            colour: None,
            bg: None,
        }
    }
}

enum Kind {
    Root,
    Text(String),
    Elem(Elem),
}

struct Node {
    kind: Kind,
    parent: usize,
    children: Vec<usize>,
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Elements whose content never reaches the reader. Everything here is
/// opaque to a text reader, so skipping to the close tag cannot swallow
/// readable content; other unusual elements stay ordinary containers so an
/// unclosed one cannot blank the rest of a message.
fn is_dropped(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "title"
            | "textarea"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "svg"
            | "math"
            | "template"
            | "applet"
    )
}

/// Open elements a new `name` implicitly closes, innermost first.
fn implied_close(name: &str) -> &'static [&'static str] {
    match name {
        "li" => &["li", "p"],
        "dt" | "dd" => &["dt", "dd", "p"],
        "tr" => &["td", "th", "tr", "p"],
        "td" | "th" => &["td", "th", "p"],
        "thead" | "tbody" | "tfoot" => &["td", "th", "tr", "thead", "tbody", "tfoot", "p"],
        "p" | "div" | "ul" | "ol" | "dl" | "table" | "blockquote" | "pre" | "hr" | "h1" | "h2"
        | "h3" | "h4" | "h5" | "h6" | "section" | "article" | "header" | "footer" | "aside"
        | "nav" | "main" | "figure" | "figcaption" | "address" | "center" | "form" | "fieldset" => {
            &["p"]
        }
        _ => &[],
    }
}

fn classify(tag: &StartTag) -> Option<Elem> {
    let name = tag.name.as_str();
    let mut elem = match name {
        "p" => Elem::simple(ElemKind::Para),
        "div" | "section" | "article" | "header" | "footer" | "aside" | "nav" | "main"
        | "figure" | "figcaption" | "address" | "center" | "form" | "fieldset" | "legend"
        | "details" | "summary" | "dialog" | "output" | "body" | "html" | "head" | "noscript" => {
            Elem::simple(ElemKind::Div)
        }
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = name
                .as_bytes()
                .get(1)
                .map(|b| b.saturating_sub(b'0'))
                .unwrap_or(1);
            Elem::simple(ElemKind::Heading(level.clamp(1, 6)))
        }
        "blockquote" => Elem::simple(ElemKind::Quote),
        "pre" | "listing" => Elem::simple(ElemKind::Pre),
        "hr" => Elem::simple(ElemKind::Rule),
        "br" => Elem::simple(ElemKind::Break),
        "ul" | "menu" | "dir" => Elem::simple(ElemKind::Ul),
        "ol" => Elem::simple(ElemKind::Ol),
        "li" => Elem::simple(ElemKind::Li),
        "dl" => Elem::simple(ElemKind::Dl),
        "dt" => {
            let mut e = Elem::simple(ElemKind::Dt);
            e.mark = "*";
            e
        }
        "dd" => Elem::simple(ElemKind::Dd),
        "table" => Elem::simple(ElemKind::Table),
        "tr" => Elem::simple(ElemKind::Row),
        "td" | "th" => Elem::simple(ElemKind::Cell),
        "thead" | "tbody" | "tfoot" | "colgroup" | "caption" => Elem::simple(ElemKind::Container),
        "a" => {
            let href = tag.attr("href").unwrap_or("").trim();
            if href.is_empty() {
                Elem::simple(ElemKind::Container)
            } else {
                let mut e = Elem::simple(ElemKind::Link);
                e.target = href.to_string();
                e
            }
        }
        "img" => {
            let src = tag.attr("src").unwrap_or("").trim();
            if src.is_empty() {
                return None;
            }
            let mut e = Elem::simple(ElemKind::Image);
            e.target = src.to_string();
            e.alt = tag.attr("alt").unwrap_or("").trim().to_string();
            e
        }
        "em" | "i" | "cite" | "var" | "dfn" => {
            let mut e = Elem::simple(ElemKind::Em);
            // html2text decorates only `<em>`.
            e.mark = if name == "em" { "*" } else { "" };
            e
        }
        "strong" | "b" => {
            let mut e = Elem::simple(ElemKind::Strong);
            e.mark = "**";
            e
        }
        "s" | "strike" | "del" => Elem::simple(ElemKind::Strike),
        "code" | "tt" | "kbd" | "samp" => {
            let mut e = Elem::simple(ElemKind::Code);
            e.mark = "`";
            e
        }
        "sup" => Elem::simple(ElemKind::Sup),
        "wbr" | "input" | "col" | "base" | "meta" | "link" | "area" | "param" | "source"
        | "track" | "embed" => return None,
        _ => Elem::simple(ElemKind::Container),
    };
    if elem.kind == ElemKind::Ol {
        if let Some(start) = tag.attr("start") {
            if let Ok(v) = start.trim().parse::<i64>() {
                elem.start = v;
            }
        }
    }
    if elem.kind == ElemKind::Cell {
        if let Some(span) = tag.attr("colspan") {
            if let Ok(v) = span.trim().parse::<usize>() {
                elem.colspan = v.clamp(1, MAX_COLS);
            }
        }
    }
    let style = tag.attr("style").unwrap_or("");
    elem.colour = style_colour(style, "color")
        .or_else(|| tag.attr("color").and_then(parse_colour))
        .or_else(|| {
            tag.attr("text")
                .filter(|_| name == "body")
                .and_then(parse_colour)
        });
    elem.bg = style_colour(style, "background-color")
        .or_else(|| style_colour(style, "background"))
        .or_else(|| tag.attr("bgcolor").and_then(parse_colour));
    Some(elem)
}

fn build(src: &str) -> Vec<Node> {
    let mut nodes = vec![Node {
        kind: Kind::Root,
        parent: 0,
        children: Vec::new(),
    }];
    let mut lex = Lexer::new(src);
    // (element name, node index if one was created)
    let mut open: Vec<(String, Option<usize>)> = Vec::new();
    let mut cur = 0usize;

    while let Some(tok) = lex.next_token() {
        match tok {
            Tok::Text(text) => {
                if text.is_empty() {
                    continue;
                }
                push_text(&mut nodes, cur, text);
            }
            Tok::Start(tag) => {
                if is_dropped(&tag.name) {
                    if !tag.self_closing {
                        lex.skip_element(&tag.name);
                    }
                    continue;
                }
                for _ in 0..open.len() {
                    let closes = implied_close(&tag.name);
                    let top = match open.last() {
                        Some((n, _)) => n.as_str(),
                        None => break,
                    };
                    if closes.contains(&top) {
                        close_top(&mut nodes, &mut open, &mut cur);
                    } else {
                        break;
                    }
                }
                let void = is_void(&tag.name) || tag.self_closing;
                let elem = classify(&tag);
                let created = match elem {
                    Some(e) if open.len() < MAX_DEPTH => Some(push_elem(&mut nodes, cur, e)),
                    _ => None,
                };
                if void {
                    continue;
                }
                if open.len() >= MAX_OPEN {
                    // `created` is None here (MAX_OPEN > MAX_DEPTH), so the
                    // element simply disappears; its children stay put.
                    continue;
                }
                if let Some(idx) = created {
                    cur = idx;
                }
                open.push((tag.name, created));
            }
            Tok::End(name) => {
                if name == "br" {
                    push_elem(&mut nodes, cur, Elem::simple(ElemKind::Break));
                    continue;
                }
                if let Some(at) = open.iter().rposition(|(n, _)| *n == name) {
                    while open.len() > at {
                        close_top(&mut nodes, &mut open, &mut cur);
                    }
                }
            }
        }
    }
    nodes
}

fn close_top(nodes: &mut [Node], open: &mut Vec<(String, Option<usize>)>, cur: &mut usize) {
    if let Some((_, Some(idx))) = open.pop() {
        *cur = nodes.get(idx).map_or(0, |n| n.parent);
    }
}

fn push_elem(nodes: &mut Vec<Node>, parent: usize, elem: Elem) -> usize {
    let idx = nodes.len();
    nodes.push(Node {
        kind: Kind::Elem(elem),
        parent,
        children: Vec::new(),
    });
    if let Some(p) = nodes.get_mut(parent) {
        p.children.push(idx);
    }
    idx
}

fn push_text(nodes: &mut Vec<Node>, parent: usize, text: String) {
    // Merge into the previous text sibling so long runs stay cheap.
    if let Some(last) = nodes.get(parent).and_then(|p| p.children.last().copied()) {
        if let Some(node) = nodes.get_mut(last) {
            if let Kind::Text(existing) = &mut node.kind {
                existing.push_str(&text);
                return;
            }
        }
    }
    let idx = nodes.len();
    nodes.push(Node {
        kind: Kind::Text(text),
        parent,
        children: Vec::new(),
    });
    if let Some(p) = nodes.get_mut(parent) {
        p.children.push(idx);
    }
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

/// Extract one declaration from a `style` attribute.
fn style_colour(style: &str, property: &str) -> Option<Rgb> {
    if style.is_empty() {
        return None;
    }
    for decl in style.split(';') {
        let Some((name, value)) = decl.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(property) {
            if let Some(c) = parse_colour(value.trim()) {
                return Some(c);
            }
        }
    }
    None
}

fn parse_colour(value: &str) -> Option<Rgb> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = value.to_ascii_lowercase();
    if let Some(args) = lower
        .strip_prefix("rgb(")
        .or_else(|| lower.strip_prefix("rgba("))
    {
        let args = args.strip_suffix(')').unwrap_or(args);
        let mut parts = args.split(',').map(str::trim);
        let r = parse_channel(parts.next()?)?;
        let g = parse_channel(parts.next()?)?;
        let b = parse_channel(parts.next()?)?;
        return Some(Rgb { r, g, b });
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return parse_hex(hex);
    }
    named_colour(&lower)
}

fn parse_channel(s: &str) -> Option<u8> {
    if let Some(pct) = s.strip_suffix('%') {
        let v: f32 = pct.trim().parse().ok()?;
        return Some((v.clamp(0.0, 100.0) * 2.55).round() as u8);
    }
    let v: f32 = s.parse().ok()?;
    Some(v.clamp(0.0, 255.0) as u8)
}

fn parse_hex(hex: &str) -> Option<Rgb> {
    let hex = hex.trim();
    let digits: Vec<u8> = hex.bytes().map(hex_val).collect::<Option<Vec<u8>>>()?;
    match digits.len() {
        3 | 4 => {
            let r = *digits.first()?;
            let g = *digits.get(1)?;
            let b = *digits.get(2)?;
            Some(Rgb {
                r: r * 17,
                g: g * 17,
                b: b * 17,
            })
        }
        6 | 8 => {
            let r = digits.first()? * 16 + digits.get(1)?;
            let g = digits.get(2)? * 16 + digits.get(3)?;
            let b = digits.get(4)? * 16 + digits.get(5)?;
            Some(Rgb { r, g, b })
        }
        _ => None,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn named_colour(name: &str) -> Option<Rgb> {
    let rgb = match name {
        "black" => 0x000000,
        "silver" => 0xc0c0c0,
        "gray" | "grey" => 0x808080,
        "white" => 0xffffff,
        "maroon" => 0x800000,
        "red" => 0xff0000,
        "purple" => 0x800080,
        "fuchsia" | "magenta" => 0xff00ff,
        "green" => 0x008000,
        "lime" => 0x00ff00,
        "olive" => 0x808000,
        "yellow" => 0xffff00,
        "navy" => 0x000080,
        "blue" => 0x0000ff,
        "teal" => 0x008080,
        "aqua" | "cyan" => 0x00ffff,
        "orange" => 0xffa500,
        "pink" => 0xffc0cb,
        "brown" => 0xa52a2a,
        "gold" => 0xffd700,
        "indigo" => 0x4b0082,
        "violet" => 0xee82ee,
        "tan" => 0xd2b48c,
        "salmon" => 0xfa8072,
        "khaki" => 0xf0e68c,
        "beige" => 0xf5f5dc,
        "ivory" => 0xfffff0,
        "lavender" => 0xe6e6fa,
        "plum" => 0xdda0dd,
        "orchid" => 0xda70d6,
        "crimson" => 0xdc143c,
        "coral" => 0xff7f50,
        "tomato" => 0xff6347,
        "chocolate" => 0xd2691e,
        "turquoise" => 0x40e0d0,
        "skyblue" => 0x87ceeb,
        "steelblue" => 0x4682b4,
        "slategray" | "slategrey" => 0x708090,
        "darkgray" | "darkgrey" => 0xa9a9a9,
        "lightgray" | "lightgrey" => 0xd3d3d3,
        "lightblue" => 0xadd8e6,
        "lightgreen" => 0x90ee90,
        "darkblue" => 0x00008b,
        "darkgreen" => 0x006400,
        "darkred" => 0x8b0000,
        "whitesmoke" => 0xf5f5f5,
        "gainsboro" => 0xdcdcdc,
        "dimgray" | "dimgrey" => 0x696969,
        "royalblue" => 0x4169e1,
        "firebrick" => 0xb22222,
        "seagreen" => 0x2e8b57,
        "goldenrod" => 0xdaa520,
        _ => return None,
    };
    Some(Rgb {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
    })
}

// ---------------------------------------------------------------------------
// Width estimation
// ---------------------------------------------------------------------------

/// Narrowest and widest column count a text run can occupy.
fn text_extent(text: &str) -> (usize, usize) {
    let mut max = 0usize;
    let mut word = 0usize;
    let mut longest = 0usize;
    let mut prev_ws = true;
    for c in text.chars() {
        if is_ignorable(c) {
            continue;
        }
        if is_html_ws(c) {
            longest = longest.max(word);
            word = 0;
            if !prev_ws {
                max += 1;
            }
            prev_ws = true;
        } else {
            let w = char_width(c);
            word += w;
            max += w;
            prev_ws = false;
        }
    }
    longest = longest.max(word);
    (longest, max)
}

fn is_inline_kind(kind: ElemKind) -> bool {
    matches!(
        kind,
        ElemKind::Container
            | ElemKind::Link
            | ElemKind::Image
            | ElemKind::Em
            | ElemKind::Strong
            | ElemKind::Strike
            | ElemKind::Code
            | ElemKind::Sup
    )
}

fn measure(nodes: &[Node], decor: Decor) -> (Vec<(usize, usize)>, Vec<bool>) {
    let n = nodes.len();
    let mut est = vec![(0usize, 0usize); n];
    let mut content = vec![false; n];
    for i in (0..n).rev() {
        let Some(node) = nodes.get(i) else { continue };
        let (mut min, mut max, has) = match &node.kind {
            Kind::Text(text) => {
                let (min, max) = text_extent(text);
                (min, max, !text.chars().all(char::is_whitespace))
            }
            Kind::Root => combine(nodes, &est, &content, i, false),
            Kind::Elem(e) => {
                let inline = is_inline_kind(e.kind);
                match e.kind {
                    ElemKind::Row => row_extent(nodes, &est, &content, i),
                    ElemKind::Table => {
                        let (min, max, has) = combine(nodes, &est, &content, i, false);
                        (min, max, has || !node.children.is_empty())
                    }
                    ElemKind::Break | ElemKind::Rule => (0, 0, true),
                    ElemKind::Image => {
                        let w = str_width(&e.alt);
                        let extra = if decor == Decor::Plain && w > 0 { 2 } else { 0 };
                        (w + extra, w + extra, w > 0)
                    }
                    _ => combine(nodes, &est, &content, i, inline),
                }
            }
        };
        if let Kind::Elem(e) = &node.kind {
            let extra = match e.kind {
                ElemKind::Quote | ElemKind::Dd => 2,
                ElemKind::Ul | ElemKind::Li => 2,
                ElemKind::Ol => 3,
                ElemKind::Heading(level) => usize::from(level) + 1,
                ElemKind::Link if decor == Decor::Plain => 5,
                _ => 0,
            };
            let marks = if decor == Decor::Plain {
                2 * e.mark.len()
            } else {
                0
            };
            min = min.saturating_add(extra).saturating_add(marks);
            max = max.saturating_add(extra).saturating_add(marks);
            if matches!(e.kind, ElemKind::Link) && !has {
                // Empty links are pruned, as html2text does.
                min = 0;
                max = 0;
            }
        }
        if let Some(slot) = est.get_mut(i) {
            *slot = (min, max);
        }
        if let Some(slot) = content.get_mut(i) {
            *slot = has;
        }
    }
    (est, content)
}

/// Combine child extents: inline children sit side by side, block children
/// stack.
fn combine(
    nodes: &[Node],
    est: &[(usize, usize)],
    content: &[bool],
    idx: usize,
    inline: bool,
) -> (usize, usize, bool) {
    let Some(node) = nodes.get(idx) else {
        return (0, 0, false);
    };
    let mut min = 0usize;
    let mut max = 0usize;
    let mut run = 0usize;
    let mut has = false;
    for &child in &node.children {
        let (cmin, cmax) = est.get(child).copied().unwrap_or((0, 0));
        has |= content.get(child).copied().unwrap_or(false);
        min = min.max(cmin);
        let child_inline = match nodes.get(child).map(|n| &n.kind) {
            Some(Kind::Text(_)) => true,
            Some(Kind::Elem(e)) => is_inline_kind(e.kind),
            _ => false,
        };
        if inline || child_inline {
            run = run.saturating_add(cmax);
            max = max.max(run);
        } else {
            run = 0;
            max = max.max(cmax);
        }
    }
    (min, max, has)
}

fn row_extent(
    nodes: &[Node],
    est: &[(usize, usize)],
    content: &[bool],
    idx: usize,
) -> (usize, usize, bool) {
    let Some(node) = nodes.get(idx) else {
        return (0, 0, false);
    };
    let mut min = 0usize;
    let mut max = 0usize;
    let mut has = false;
    let mut cells = 0usize;
    for &child in &node.children {
        let (cmin, cmax) = est.get(child).copied().unwrap_or((0, 0));
        has |= content.get(child).copied().unwrap_or(false);
        min = min.saturating_add(cmin);
        max = max.saturating_add(cmax);
        cells += 1;
    }
    let seps = cells.saturating_sub(1);
    (min.saturating_add(seps), max.saturating_add(seps), has)
}

// ---------------------------------------------------------------------------
// Line buffer and wrapping
// ---------------------------------------------------------------------------

struct Out {
    width: usize,
    lines: Vec<Vec<Span>>,
    inline: Vec<Span>,
    /// Set when a block ends: whitespace before the next content is dropped
    /// and a blank line separates it.
    at_block_end: bool,
    /// The pending line came from `<pre>` and must not be wrapped.
    verbatim: bool,
    /// Whether any line emitted so far carries text.
    seen_content: bool,
}

impl Out {
    fn new(width: usize) -> Out {
        Out {
            width,
            lines: Vec::new(),
            inline: Vec::new(),
            at_block_end: false,
            verbatim: false,
            seen_content: false,
        }
    }

    fn emit(&mut self, line: Vec<Span>) {
        self.seen_content |= line_has_content(&line);
        self.lines.push(line);
    }

    fn ends_with_space(&self) -> bool {
        match self.inline.last() {
            None => true,
            Some(span) => span.text.ends_with(' '),
        }
    }

    fn push_span(&mut self, text: String, tags: &[Tag]) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.inline.last_mut() {
            if last.tags == tags {
                last.text.push_str(&text);
                return;
            }
        }
        self.inline.push(Span {
            text,
            tags: tags.to_vec(),
        });
    }

    /// Add collapsed inline text.
    fn add_text(&mut self, text: &str, tags: &[Tag], strike: bool) {
        if text.is_empty() {
            return;
        }
        if self.at_block_end {
            if text.chars().all(char::is_whitespace) {
                return;
            }
            self.start_block();
        }
        let mut buf = String::with_capacity(text.len());
        let mut prev_ws = self.ends_with_space();
        for c in text.chars() {
            if is_ignorable(c) {
                continue;
            }
            if is_html_ws(c) {
                if !prev_ws {
                    buf.push(' ');
                    prev_ws = true;
                }
            } else {
                buf.push(c);
                if strike {
                    buf.push(STRIKE);
                }
                prev_ws = false;
            }
        }
        self.push_span(buf, tags);
    }

    /// Add verbatim text, honouring embedded newlines and tabs.
    fn add_pre(&mut self, text: &str, tags: &[Tag], strike: bool) {
        if self.at_block_end && !text.is_empty() {
            self.start_block();
        }
        let mut first = true;
        for part in text.split('\n') {
            if !first {
                self.flush_line();
            }
            first = false;
            self.verbatim = true;
            let mut buf = String::with_capacity(part.len());
            let mut col = self
                .inline
                .iter()
                .map(|s| str_width(&s.text))
                .sum::<usize>();
            for c in part.chars() {
                if c == '\r' || is_ignorable(c) {
                    continue;
                }
                if c == '\t' {
                    let pad = TAB_STOP - (col % TAB_STOP);
                    for _ in 0..pad {
                        buf.push(' ');
                    }
                    col += pad;
                    continue;
                }
                buf.push(c);
                if strike {
                    buf.push(STRIKE);
                }
                col += char_width(c);
            }
            self.push_span(buf, tags);
        }
    }

    /// Wrap and emit whatever is pending.
    fn flush_line(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.inline);
        if self.verbatim {
            self.verbatim = false;
            self.emit(spans);
            return;
        }
        for line in wrap(spans, self.width) {
            self.emit(line);
        }
    }

    fn new_line(&mut self) {
        self.flush_line();
    }

    /// Separate blocks: an empty line unless nothing has been emitted yet.
    fn start_block(&mut self) {
        self.at_block_end = false;
        self.flush_line();
        if self.seen_content {
            self.lines.push(Vec::new());
        }
    }

    fn end_block(&mut self) {
        self.at_block_end = true;
    }

    /// `<br>`: end the line, or emit an empty one if the line is empty.
    fn hard_break(&mut self) {
        if self.at_block_end {
            self.at_block_end = false;
        }
        if self.inline.is_empty() {
            self.lines.push(Vec::new());
        } else {
            self.flush_line();
        }
    }

    fn push_line(&mut self, line: Vec<Span>) {
        self.flush_line();
        self.at_block_end = false;
        self.emit(line);
    }

    /// Append an already rendered block, prefixing its lines.
    fn append_sub(&mut self, sub: Out, first: &str, cont: &str, tags: &[Tag]) {
        self.flush_line();
        let sub_lines = sub.into_lines();
        if sub_lines.is_empty() {
            return;
        }
        self.at_block_end = false;
        for (i, line) in sub_lines.into_iter().enumerate() {
            let prefix = if i == 0 { first } else { cont };
            let mut out_line = Vec::with_capacity(line.len() + 1);
            if !prefix.is_empty() {
                out_line.push(Span {
                    text: prefix.to_string(),
                    tags: tags.to_vec(),
                });
            }
            out_line.extend(line);
            self.emit(out_line);
        }
    }

    fn into_lines(mut self) -> Vec<Vec<Span>> {
        self.flush_line();
        self.lines
    }
}

/// One piece of a wrapped line: text plus the index of the source span whose
/// tags it carries.
struct Piece {
    text: String,
    src: usize,
}

fn push_piece(pieces: &mut Vec<Piece>, src: usize, c: char) {
    if let Some(last) = pieces.last_mut() {
        if last.src == src {
            last.text.push(c);
            return;
        }
    }
    let mut text = String::new();
    text.push(c);
    pieces.push(Piece { text, src });
}

fn materialise(pieces: Vec<Piece>, spans: &[Span]) -> Vec<Span> {
    pieces
        .into_iter()
        .map(|p| Span {
            text: p.text,
            tags: spans.get(p.src).map(|s| s.tags.clone()).unwrap_or_default(),
        })
        .collect()
}

/// Greedy word wrapping. Words longer than the width are split; a width of 0
/// means no wrapping.
fn wrap(spans: Vec<Span>, width: usize) -> Vec<Vec<Span>> {
    let mut lines: Vec<Vec<Span>> = Vec::new();
    let mut cur: Vec<Piece> = Vec::new();
    let mut cur_w = 0usize;
    let mut word: Vec<Piece> = Vec::new();
    let mut word_w = 0usize;
    let mut space: Option<usize> = None;

    for (src, span) in spans.iter().enumerate() {
        for c in span.text.chars() {
            if c == ' ' {
                if !word.is_empty() {
                    flush_word(
                        &mut lines,
                        &mut cur,
                        &mut cur_w,
                        &mut word,
                        &mut word_w,
                        &mut space,
                        width,
                        &spans,
                    );
                }
                if !cur.is_empty() || !lines.is_empty() || word_w > 0 {
                    space = Some(src);
                }
                continue;
            }
            push_piece(&mut word, src, c);
            word_w += char_width(c);
        }
    }
    flush_word(
        &mut lines,
        &mut cur,
        &mut cur_w,
        &mut word,
        &mut word_w,
        &mut space,
        width,
        &spans,
    );
    if !cur.is_empty() {
        lines.push(materialise(cur, &spans));
    }
    lines
}

#[allow(clippy::too_many_arguments)]
fn flush_word(
    lines: &mut Vec<Vec<Span>>,
    cur: &mut Vec<Piece>,
    cur_w: &mut usize,
    word: &mut Vec<Piece>,
    word_w: &mut usize,
    space: &mut Option<usize>,
    width: usize,
    spans: &[Span],
) {
    if word.is_empty() {
        *space = None;
        return;
    }
    let sep = usize::from(!cur.is_empty());
    let fits = width == 0 || *cur_w + sep + *word_w <= width;
    if fits {
        if sep == 1 {
            let src = space.unwrap_or_else(|| cur.last().map_or(0, |p| p.src));
            push_piece(cur, src, ' ');
            *cur_w += 1;
        }
        cur.append(word);
        *cur_w += *word_w;
        *word_w = 0;
        *space = None;
        return;
    }
    if !cur.is_empty() {
        lines.push(materialise(std::mem::take(cur), spans));
        *cur_w = 0;
    }
    // The word starts a fresh line; split it if it still does not fit.
    if width > 0 && *word_w > width {
        let (full, rest, rest_w) = split_word(std::mem::take(word), width);
        for line in full {
            lines.push(materialise(line, spans));
        }
        *cur = rest;
        *cur_w = rest_w;
    } else {
        *cur = std::mem::take(word);
        *cur_w = *word_w;
    }
    *word_w = 0;
    *space = None;
}

/// Break an over-long word into width-sized lines, returning the completed
/// lines plus the remainder and its width.
fn split_word(word: Vec<Piece>, width: usize) -> (Vec<Vec<Piece>>, Vec<Piece>, usize) {
    let mut lines: Vec<Vec<Piece>> = Vec::new();
    let mut cur: Vec<Piece> = Vec::new();
    let mut cur_w = 0usize;
    for piece in word {
        for c in piece.text.chars() {
            let cw = char_width(c);
            if cur_w > 0 && cur_w + cw > width {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            push_piece(&mut cur, piece.src, c);
            cur_w += cw;
        }
    }
    (lines, cur, cur_w)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct Rend<'a> {
    nodes: &'a [Node],
    est: Vec<(usize, usize)>,
    content: Vec<bool>,
    decor: Decor,
    tags: Vec<Tag>,
    links: Vec<String>,
    strike: usize,
    pre: usize,
    table_depth: usize,
}

struct Cell {
    node: usize,
    colspan: usize,
}

impl<'a> Rend<'a> {
    fn plain(&self) -> bool {
        self.decor == Decor::Plain
    }

    fn has_content(&self, idx: usize) -> bool {
        self.content.get(idx).copied().unwrap_or(false)
    }

    fn extent(&self, idx: usize) -> (usize, usize) {
        self.est.get(idx).copied().unwrap_or((0, 0))
    }

    fn children(&mut self, idx: usize, out: &mut Out) {
        let nodes = self.nodes;
        let Some(node) = nodes.get(idx) else { return };
        for k in 0..node.children.len() {
            let Some(&child) = node.children.get(k) else {
                break;
            };
            self.node(child, out);
        }
    }

    fn node(&mut self, idx: usize, out: &mut Out) {
        let nodes = self.nodes;
        let Some(node) = nodes.get(idx) else { return };
        match &node.kind {
            Kind::Root => self.children(idx, out),
            Kind::Text(text) => {
                if self.pre > 0 {
                    out.add_pre(text, &self.tags, self.strike > 0);
                } else {
                    out.add_text(text, &self.tags, self.strike > 0);
                }
            }
            Kind::Elem(elem) => self.elem(idx, elem, out),
        }
    }

    fn elem(&mut self, idx: usize, elem: &'a Elem, out: &mut Out) {
        let mut pushed = 0usize;
        if let Some(c) = elem.colour {
            self.tags.push(Tag::Colour(c));
            pushed += 1;
        }
        if let Some(c) = elem.bg {
            self.tags.push(Tag::BgColour(c));
            pushed += 1;
        }
        match elem.kind {
            ElemKind::Container => self.children(idx, out),
            ElemKind::Para => {
                if self.has_content(idx) {
                    out.start_block();
                    self.children(idx, out);
                    out.end_block();
                }
            }
            ElemKind::Div => {
                if self.has_content(idx) {
                    out.new_line();
                    self.children(idx, out);
                    out.new_line();
                }
            }
            ElemKind::Heading(level) => {
                if self.has_content(idx) {
                    let prefix = format!("{} ", "#".repeat(usize::from(level)));
                    out.start_block();
                    self.sub_block(idx, out, &prefix, &prefix);
                    out.end_block();
                }
            }
            ElemKind::Quote => {
                if self.has_content(idx) {
                    out.start_block();
                    self.sub_block(idx, out, "> ", "> ");
                    out.end_block();
                }
            }
            ElemKind::Pre => {
                out.start_block();
                self.tags.push(Tag::Preformat);
                self.pre += 1;
                self.children(idx, out);
                self.pre -= 1;
                self.tags.pop();
                out.end_block();
            }
            ElemKind::Rule => {
                let w = if out.width == 0 { 3 } else { out.width };
                out.push_line(vec![Span::plain("-".repeat(w))]);
            }
            ElemKind::Break => out.hard_break(),
            ElemKind::Ul => self.list(idx, out, None),
            ElemKind::Ol => self.list(idx, out, Some(elem.start)),
            ElemKind::Li | ElemKind::Dl => {
                // A stray item outside its list renders as a block.
                if self.has_content(idx) {
                    out.new_line();
                    self.children(idx, out);
                    out.new_line();
                }
            }
            ElemKind::Dt => {
                out.new_line();
                self.marked(idx, out, elem.mark, Some(Tag::Emphasis));
                out.new_line();
            }
            ElemKind::Dd => {
                self.sub_block(idx, out, "  ", "  ");
            }
            ElemKind::Table => self.table(idx, out),
            ElemKind::Row | ElemKind::Cell => {
                // Outside a laid-out table these are just blocks.
                if self.has_content(idx) {
                    out.new_line();
                    self.children(idx, out);
                    out.new_line();
                }
            }
            ElemKind::Link => self.link(idx, elem, out),
            ElemKind::Image => {
                if !elem.alt.is_empty() {
                    self.tags.push(Tag::Image(elem.target.clone()));
                    let text = if self.plain() {
                        format!("[{}]", elem.alt)
                    } else {
                        elem.alt.clone()
                    };
                    let tags = self.tags.clone();
                    out.add_text(&text, &tags, false);
                    self.tags.pop();
                }
            }
            ElemKind::Em => self.marked(idx, out, elem.mark, Some(Tag::Emphasis)),
            ElemKind::Strong => self.marked(idx, out, elem.mark, Some(Tag::Strong)),
            ElemKind::Code => self.marked(idx, out, elem.mark, Some(Tag::Code)),
            ElemKind::Strike => {
                self.tags.push(Tag::Strikeout);
                self.strike += 1;
                self.children(idx, out);
                self.strike -= 1;
                self.tags.pop();
            }
            ElemKind::Sup => self.superscript(idx, out),
        }
        for _ in 0..pushed {
            self.tags.pop();
        }
    }

    /// Render children with a tag and, in plain mode, literal markers.
    fn marked(&mut self, idx: usize, out: &mut Out, mark: &str, tag: Option<Tag>) {
        let tagged = tag.is_some();
        if let Some(t) = tag {
            self.tags.push(t);
        }
        let decorate = self.plain() && !mark.is_empty() && self.has_content(idx);
        if decorate {
            let tags = self.tags.clone();
            out.add_text(mark, &tags, false);
        }
        self.children(idx, out);
        if decorate {
            let tags = self.tags.clone();
            out.add_text(mark, &tags, false);
        }
        if tagged {
            self.tags.pop();
        }
    }

    fn link(&mut self, idx: usize, elem: &'a Elem, out: &mut Out) {
        if !self.has_content(idx) {
            // html2text prunes links with no anchor text.
            return;
        }
        self.tags.push(Tag::Link(elem.target.clone()));
        if self.plain() {
            let tags = self.tags.clone();
            out.add_text("[", &tags, false);
        }
        self.children(idx, out);
        if self.plain() {
            let tags = self.tags.clone();
            out.add_text("]", &tags, false);
        }
        self.tags.pop();
        if self.plain() {
            self.links.push(elem.target.clone());
            let marker = format!("[{}]", self.links.len());
            let tags = self.tags.clone();
            out.add_text(&marker, &tags, false);
        }
    }

    fn superscript(&mut self, idx: usize, out: &mut Out) {
        let text = self.subtree_text(idx);
        if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) {
            const SUPS: [char; 10] = ['⁰', '¹', '²', '³', '⁴', '⁵', '⁶', '⁷', '⁸', '⁹'];
            let mut buf = String::new();
            for c in text.chars() {
                let i = (c as usize).saturating_sub('0' as usize);
                if let Some(&s) = SUPS.get(i) {
                    buf.push(s);
                }
            }
            let tags = self.tags.clone();
            out.add_text(&buf, &tags, false);
            return;
        }
        let tags = self.tags.clone();
        out.add_text("^{", &tags, false);
        self.children(idx, out);
        let tags = self.tags.clone();
        out.add_text("}", &tags, false);
    }

    fn subtree_text(&self, idx: usize) -> String {
        let mut buf = String::new();
        let mut stack = vec![idx];
        let mut budget = 4096usize;
        while let Some(node_idx) = stack.pop() {
            let Some(node) = self.nodes.get(node_idx) else {
                continue;
            };
            if let Kind::Text(t) = &node.kind {
                buf.push_str(t);
                if buf.len() > budget {
                    break;
                }
            }
            budget = budget.saturating_sub(1);
            if budget == 0 {
                break;
            }
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
        }
        buf.trim().to_string()
    }

    /// Render children into a narrower block and append it with a prefix.
    /// Once nesting has eaten the whole width the prefix is dropped rather
    /// than pushing lines past it.
    fn sub_block(&mut self, idx: usize, out: &mut Out, first: &str, cont: &str) {
        let indent = str_width(first);
        if out.width != 0 && indent >= out.width {
            out.new_line();
            self.children(idx, out);
            out.new_line();
            return;
        }
        let sub = self.sub_render(idx, out.width, indent);
        out.append_sub(sub, first, cont, &self.tags);
    }

    fn sub_render(&mut self, idx: usize, width: usize, indent: usize) -> Out {
        let inner = if width == 0 {
            0
        } else {
            width.saturating_sub(indent).max(1)
        };
        let mut sub = Out::new(inner);
        self.children(idx, &mut sub);
        sub
    }

    fn list(&mut self, idx: usize, out: &mut Out, start: Option<i64>) {
        let nodes = self.nodes;
        let Some(node) = nodes.get(idx) else { return };
        let items: Vec<usize> = node
            .children
            .iter()
            .copied()
            .filter(|&c| matches!(nodes.get(c).map(|n| &n.kind), Some(Kind::Elem(e)) if e.kind == ElemKind::Li))
            .collect();
        if items.is_empty() {
            // A list with no items still shows any stray content.
            self.children(idx, out);
            return;
        }
        let prefix_width = match start {
            None => 2,
            Some(first) => {
                let last = first.saturating_add(items.len() as i64).saturating_sub(1);
                let a = format!("{}. ", first).len();
                let b = format!("{}. ", last).len();
                a.max(b)
            }
        };
        if out.width != 0 && prefix_width >= out.width {
            for &item in &items {
                out.new_line();
                self.children(item, out);
                out.new_line();
            }
            return;
        }
        let indent = " ".repeat(prefix_width);
        for (i, &item) in items.iter().enumerate() {
            let prefix = match start {
                None => "* ".to_string(),
                Some(first) => {
                    let n = first.saturating_add(i as i64);
                    format!("{:<width$}", format!("{}. ", n), width = prefix_width)
                }
            };
            let sub = self.sub_render(item, out.width, prefix_width);
            out.append_sub(sub, &prefix, &indent, &self.tags);
        }
    }

    /// Collect the cells of every row, descending through `thead`/`tbody`.
    fn collect_rows(&self, idx: usize) -> Vec<Vec<Cell>> {
        let nodes = self.nodes;
        let mut rows: Vec<Vec<Cell>> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        if let Some(node) = nodes.get(idx) {
            stack.extend(node.children.iter().rev().copied());
        }
        let mut loose: Vec<Cell> = Vec::new();
        while let Some(child) = stack.pop() {
            let Some(node) = nodes.get(child) else {
                continue;
            };
            let Kind::Elem(e) = &node.kind else { continue };
            match e.kind {
                ElemKind::Row => {
                    let cells: Vec<Cell> = node
                        .children
                        .iter()
                        .filter_map(|&c| match nodes.get(c).map(|n| &n.kind) {
                            Some(Kind::Elem(ce)) if ce.kind == ElemKind::Cell => Some(Cell {
                                node: c,
                                colspan: ce.colspan.max(1),
                            }),
                            _ => None,
                        })
                        .collect();
                    if !cells.is_empty() {
                        rows.push(cells);
                    }
                }
                ElemKind::Cell => loose.push(Cell {
                    node: child,
                    colspan: e.colspan.max(1),
                }),
                ElemKind::Container => {
                    // thead/tbody/tfoot and other wrappers.
                    let mut kids: Vec<usize> = node.children.clone();
                    kids.reverse();
                    stack.extend(kids);
                }
                _ => {}
            }
        }
        if !loose.is_empty() {
            rows.push(loose);
        }
        rows
    }

    fn table(&mut self, idx: usize, out: &mut Out) {
        let rows = self.collect_rows(idx);
        let ncols: usize = rows
            .iter()
            .map(|r| r.iter().map(|c| c.colspan).sum::<usize>())
            .max()
            .unwrap_or(0);
        if rows.is_empty() || ncols == 0 || ncols > MAX_COLS || self.table_depth >= MAX_TABLE_DEPTH
        {
            // Degrade to stacked blocks rather than laying out something
            // unreadable or unbounded.
            if self.has_content(idx) {
                out.new_line();
                self.children(idx, out);
                out.new_line();
            }
            return;
        }
        let widths = self.column_widths(&rows, ncols, out.width);
        out.start_block();
        out.push_line(vec![Span::plain(border(&widths, '┬'))]);
        self.table_depth += 1;
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                out.push_line(vec![Span::plain(border(&widths, '┼'))]);
            }
            self.row(row, &widths, out);
        }
        self.table_depth -= 1;
        out.push_line(vec![Span::plain(border(&widths, '┴'))]);
    }

    fn column_widths(&self, rows: &[Vec<Cell>], ncols: usize, width: usize) -> Vec<usize> {
        let mut min = vec![0usize; ncols];
        let mut max = vec![0usize; ncols];
        for row in rows {
            let mut col = 0usize;
            for cell in row {
                let (cmin, cmax) = self.extent(cell.node);
                if cell.colspan == 1 {
                    if let (Some(a), Some(b)) = (min.get_mut(col), max.get_mut(col)) {
                        *a = (*a).max(cmin);
                        *b = (*b).max(cmax);
                    }
                }
                col += cell.colspan;
            }
        }
        // Spanning cells only widen columns that are still too narrow.
        for row in rows {
            let mut col = 0usize;
            for cell in row {
                if cell.colspan > 1 {
                    let span = cell.colspan.min(ncols.saturating_sub(col));
                    if span > 1 {
                        let (cmin, cmax) = self.extent(cell.node);
                        grow(&mut min, col, span, cmin);
                        grow(&mut max, col, span, cmax);
                    }
                }
                col += cell.colspan;
            }
        }
        for i in 0..ncols {
            if let Some(m) = max.get_mut(i) {
                *m = (*m).max(1);
            }
            if let (Some(a), Some(b)) = (min.get(i).copied(), max.get_mut(i)) {
                *b = (*b).max(a.min(*b).max(1));
            }
            if let Some(a) = min.get_mut(i) {
                *a = (*a).max(1);
            }
        }
        let seps = ncols.saturating_sub(1);
        let natural: usize = max.iter().copied().fold(0usize, usize::saturating_add);
        if width == 0 || natural.saturating_add(seps) <= width {
            return max;
        }
        let avail = width.saturating_sub(seps).max(ncols);
        // Shrink proportionally, never below one column per cell.
        let mut widths: Vec<usize> = max
            .iter()
            .map(|&m| {
                let scaled = m.saturating_mul(avail) / natural.max(1);
                scaled.max(1)
            })
            .collect();
        let mut used: usize = widths.iter().sum();
        while used > avail {
            let Some((i, _)) = widths
                .iter()
                .enumerate()
                .filter(|(_, &w)| w > 1)
                .max_by_key(|(_, &w)| w)
            else {
                break;
            };
            if let Some(w) = widths.get_mut(i) {
                *w -= 1;
            }
            used -= 1;
        }
        while used < avail {
            let Some((i, _)) = widths
                .iter()
                .enumerate()
                .filter(|(i, &w)| w < max.get(*i).copied().unwrap_or(w))
                .min_by_key(|(_, &w)| w)
            else {
                break;
            };
            if let Some(w) = widths.get_mut(i) {
                *w += 1;
            }
            used += 1;
        }
        widths
    }

    fn row(&mut self, row: &[Cell], widths: &[usize], out: &mut Out) {
        let mut rendered: Vec<(Vec<Vec<Span>>, usize)> = Vec::with_capacity(row.len());
        let mut col = 0usize;
        for cell in row {
            let span = cell.colspan.max(1);
            let mut w = 0usize;
            for k in 0..span {
                if let Some(cw) = widths.get(col + k) {
                    w += cw + usize::from(k > 0);
                }
            }
            let w = w.max(1);
            let mut sub = Out::new(w);
            // Cell colours are pushed here because the layout path bypasses
            // the ordinary element handler.
            let mut pushed = 0usize;
            if let Some(Kind::Elem(e)) = self.nodes.get(cell.node).map(|n| &n.kind) {
                if let Some(c) = e.colour {
                    self.tags.push(Tag::Colour(c));
                    pushed += 1;
                }
                if let Some(c) = e.bg {
                    self.tags.push(Tag::BgColour(c));
                    pushed += 1;
                }
            }
            self.children(cell.node, &mut sub);
            for _ in 0..pushed {
                self.tags.pop();
            }
            rendered.push((sub.into_lines(), w));
            col += span;
        }
        let height = rendered.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
        for r in 0..height.max(1) {
            let mut line: Vec<Span> = Vec::new();
            for (i, (cell_lines, w)) in rendered.iter().enumerate() {
                if i > 0 {
                    line.push(Span::plain("│".to_string()));
                }
                let empty: Vec<Span> = Vec::new();
                let cell_line = cell_lines.get(r).unwrap_or(&empty);
                let mut used = 0usize;
                for span in cell_line {
                    used += str_width(&span.text);
                    line.push(span.clone());
                }
                if used < *w {
                    line.push(Span::plain(" ".repeat(w - used)));
                }
            }
            out.push_line(line);
        }
    }
}

fn grow(widths: &mut [usize], col: usize, span: usize, needed: usize) {
    let current: usize = (0..span)
        .filter_map(|k| widths.get(col + k).copied())
        .fold(0usize, usize::saturating_add)
        .saturating_add(span.saturating_sub(1));
    if current >= needed {
        return;
    }
    let mut deficit = needed - current;
    let mut k = 0usize;
    while deficit > 0 && span > 0 {
        if let Some(w) = widths.get_mut(col + (k % span)) {
            *w += 1;
            deficit -= 1;
        } else {
            break;
        }
        k += 1;
    }
}

fn border(widths: &[usize], join: char) -> String {
    let mut s = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            s.push(join);
        }
        for _ in 0..*w {
            s.push('─');
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(html: &str, width: usize) -> String {
        to_text(html.as_bytes(), width)
    }

    /// Flatten rich lines the way `to_text` flattens its own.
    fn flatten(lines: &[Vec<Span>]) -> String {
        let mut out = String::new();
        for line in lines {
            for span in line {
                out.push_str(&span.text);
            }
            out.push('\n');
        }
        out
    }

    /// td-news's own filter for the link reference lines this renderer appends;
    /// copied from news-td so the format stays compatible.
    fn is_reference_link_def(line: &str) -> bool {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            return false;
        }
        if let Some(bracket_end) = trimmed.find("]: ") {
            let label = trimmed.get(1..bracket_end).unwrap_or("");
            label.chars().all(|c| c.is_ascii_digit()) && !label.is_empty()
        } else {
            false
        }
    }

    // -- entities ----------------------------------------------------------

    #[test]
    fn entity_table_is_sorted_and_large() {
        assert!(ENTITIES.len() >= 300, "have {}", ENTITIES.len());
        for pair in ENTITIES.windows(2) {
            let (a, b) = (pair[0].0, pair[1].0);
            assert!(a < b, "{a} not before {b}");
        }
        assert!(ENTITIES.iter().filter(|e| e.2).count() >= 100);
    }

    #[test]
    fn named_entities_decode() {
        assert_eq!(text("&amp;&lt;&gt;&quot;&apos;", 80), "&<>\"'\n");
        assert_eq!(
            text("&copy;&reg;&trade;&mdash;&ndash;&hellip;", 80),
            "©®™—–…\n"
        );
        assert_eq!(
            text(
                "&laquo;&raquo;&ldquo;&rdquo;&lsquo;&rsquo;&bull;&middot;",
                80
            ),
            "«»“”‘’•·\n"
        );
        assert_eq!(
            text("&euro;&pound;&yen;&cent;&deg;&plusmn;&times;&divide;", 80),
            "€£¥¢°±×÷\n"
        );
        assert_eq!(text("&frac12;&frac14;&frac34;&sup2;&sup3;", 80), "½¼¾²³\n");
        assert_eq!(text("&para;&sect;&dagger;&permil;", 80), "¶§†‰\n");
        assert_eq!(text("&eacute;&Uuml;&ntilde;&ccedil;&szlig;", 80), "éÜñçß\n");
        assert_eq!(text("&alpha;&beta;&Omega;&pi;&mu;", 80), "αβΩπμ\n");
        assert_eq!(text("&larr;&rarr;&uarr;&darr;&harr;", 80), "←→↑↓↔\n");
        assert_eq!(
            text("&infin;&ne;&le;&ge;&asymp;&sum;&radic;", 80),
            "∞≠≤≥≈∑√\n"
        );
    }

    #[test]
    fn invisible_entities_keep_their_place() {
        // Soft hyphen and the zero-width joiners are zero columns wide but
        // still present; nbsp is content, not a wrap point.
        assert_eq!(
            text("a&shy;b&zwnj;c&zwj;d", 80),
            "a\u{ad}b\u{200c}c\u{200d}d\n"
        );
        assert_eq!(text("a&nbsp;b", 80), "a\u{a0}b\n");
        assert_eq!(str_width("a\u{ad}b\u{200c}c"), 3);
    }

    #[test]
    fn unicode_spaces_collapse_but_nbsp_does_not() {
        assert_eq!(text("a&ensp;&emsp;&thinsp;b", 80), "a b\n");
        assert_eq!(text("a&nbsp;&nbsp;b", 80), "a\u{a0}\u{a0}b\n");
    }

    #[test]
    fn legacy_entities_without_semicolon() {
        assert_eq!(text("&amp &copy &nbsp &lt &gt", 80), "& © \u{a0} < >\n");
        // The longest legacy name wins, and the tail stays text.
        assert_eq!(text("&notit;", 80), "¬it;\n");
    }

    #[test]
    fn unknown_entities_are_left_alone() {
        assert_eq!(text("&unknown; &amp", 80), "&unknown; &\n");
        assert_eq!(text("a & b", 80), "a & b\n");
        // No double decoding.
        assert_eq!(text("&#38;#38;", 80), "&#38;\n");
    }

    #[test]
    fn numeric_entities_decode() {
        assert_eq!(text("&#65;&#x42;&#x1F600;", 80), "AB😀\n");
        // Browsers map the C1 range through Windows-1252.
        assert_eq!(text("&#146;&#151;&#128;", 80), "’—€\n");
        // Out of range values become the replacement character.
        assert_eq!(
            text("&#0;&#xD800;&#9999999;", 80),
            "\u{fffd}\u{fffd}\u{fffd}\n"
        );
        assert_eq!(text("&#xZZ;", 80), "&#xZZ;\n");
    }

    // -- tokenizer ---------------------------------------------------------

    #[test]
    fn comments_cdata_and_doctype_are_dropped() {
        assert_eq!(
            text(
                "<!DOCTYPE html><!-- hi --><p>a<!--\n-->b</p><![CDATA[x]]>",
                80
            ),
            "ab\n"
        );
        // An unterminated comment swallows the rest, as browsers do.
        assert_eq!(text("<p>a</p><!-- unterminated", 80), "a\n");
    }

    #[test]
    fn script_style_head_and_title_are_dropped() {
        let html = "<html><head><title>Doc</title><style>p{color:red}</style></head>\
                    <body><script>if (a < b) { x() }</script><p>text</p></body></html>";
        assert_eq!(text(html, 80), "text\n");
    }

    #[test]
    fn attributes_in_any_quoting() {
        let html = "<a href=\"https://a.example/1\">d</a> \
                    <a href='https://a.example/2'>s</a> \
                    <a href=https://a.example/3>b</a> \
                    <span class=x hidden data-y='1'>plain</span>";
        assert_eq!(
            text(html, 80),
            "[d][1] [s][2] [b][3] plain\n\n\
             [1]: https://a.example/1\n\
             [2]: https://a.example/2\n\
             [3]: https://a.example/3\n"
        );
    }

    #[test]
    fn malformed_markup_never_panics() {
        for html in [
            "<",
            "<<<>>>",
            "< p >text",
            "<p",
            "<p class=",
            "<p class=\"unterminated",
            "</>",
            "</p>",
            "<a href=>x</a>",
            "<img>",
            "<table><tr><td>",
            "<ul><li>",
            "&#",
            "&#x",
            "&",
            "<!--",
            "<![CDATA[",
            "<svg>",
            "<pre>",
        ] {
            let _ = text(html, 20);
            let _ = to_rich(html.as_bytes(), 20);
        }
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily() {
        let bytes = b"<p>caf\xff\xfe</p>";
        assert!(to_text(bytes, 80).starts_with("caf"));
    }

    #[test]
    fn misnested_and_unclosed_tags() {
        // `</b>` closes the open `<i>` with it; the stray `</i>` is ignored.
        assert_eq!(
            text("<p>a<b>bo<i>ld</b>it</i>done</p>", 40),
            "a**bold**itdone\n"
        );
        assert_eq!(text("<p>one<p>two<p>three", 20), "one\n\ntwo\n\nthree\n");
        assert_eq!(text("<ul><li>one<li>two</ul>", 20), "* one\n* two\n");
    }

    #[test]
    fn bare_angle_brackets_are_text() {
        assert_eq!(text("a < b > c", 80), "a < b > c\n");
        assert_eq!(text("1<2 and 3>2", 80), "1<2 and 3>2\n");
    }

    // -- block layout ------------------------------------------------------

    #[test]
    fn paragraphs_are_separated_by_a_blank_line() {
        assert_eq!(text("<p>Hello</p><p>World</p>", 20), "Hello\n\nWorld\n");
        // Divs only break the line.
        assert_eq!(
            text("<p>Hello</p><div>Div</div><div>Div2</div>", 20),
            "Hello\n\nDiv\nDiv2\n"
        );
        // Empty blocks contribute nothing.
        assert_eq!(text("<p>a</p><p></p><div>  </div><p>b</p>", 20), "a\n\nb\n");
    }

    #[test]
    fn whitespace_collapses_outside_pre() {
        assert_eq!(
            text("<p>\n   One\n   Two\n   Three\n</p>", 20),
            "One Two Three\n"
        );
        assert_eq!(
            text("  leading and trailing   ", 20),
            "leading and trailing\n"
        );
    }

    #[test]
    fn headings_use_hash_prefixes() {
        assert_eq!(text("<h1>Title</h1><p>x</p>", 20), "# Title\n\nx\n");
        assert_eq!(text("<h3>Sub</h3>", 20), "### Sub\n");
        assert_eq!(
            text("<h2>A rather long heading here</h2>", 12),
            "## A rather\n## long\n## heading\n## here\n"
        );
    }

    #[test]
    fn blockquote_is_prefixed() {
        assert_eq!(
            text(
                "<p>Hello</p><blockquote>One, two, three</blockquote><p>foo</p>",
                12
            ),
            "Hello\n\n> One, two,\n> three\n\nfoo\n"
        );
        assert_eq!(
            text("<blockquote><p>q1</p><p>q2</p></blockquote>", 20),
            "> q1\n> \n> q2\n"
        );
    }

    #[test]
    fn horizontal_rule_is_a_line_of_dashes() {
        assert_eq!(text("<p>a</p><hr><p>b</p>", 8), "a\n--------\n\nb\n");
        assert_eq!(text("<p>a</p><hr>", 0), "a\n---\n");
    }

    #[test]
    fn breaks_end_the_line() {
        assert_eq!(text("<p>Hello<br>World</p>", 20), "Hello\nWorld\n");
        assert_eq!(text("<p>Hello<br><br>World</p>", 20), "Hello\n\nWorld\n");
        assert_eq!(text("<p>Hello<br> <br>World</p>", 20), "Hello\n\nWorld\n");
    }

    #[test]
    fn unordered_lists() {
        assert_eq!(
            text(
                "<ul><li>Item one</li><li>Item two</li><li>Item three</li></ul>",
                10
            ),
            "* Item one\n* Item two\n* Item\n  three\n"
        );
    }

    #[test]
    fn nested_lists_indent() {
        let html = "<ul><li>Item 1</li><li>Item 2<ul><li>SubItem 2.1</li>\
                    <li>SubItem 2.2<ul><li>Sub Item 2.2.1</li></ul></li></ul></li></ul>";
        assert_eq!(
            text(html, 80),
            "* Item 1\n* Item 2\n  * SubItem 2.1\n  * SubItem 2.2\n    * Sub Item 2.2.1\n"
        );
        let html = "<ol><li>Item 1</li><li>Item 2<ol><li>SubItem 2.1</li></ol></li></ol>";
        assert_eq!(text(html, 80), "1. Item 1\n2. Item 2\n   1. SubItem 2.1\n");
    }

    #[test]
    fn ordered_lists_pad_their_numbers() {
        let mut html = String::from("<ol>");
        for i in 1..=10 {
            html.push_str(&format!("<li>Item {}</li>", i));
        }
        html.push_str("</ol>");
        let out = text(&html, 20);
        assert!(out.starts_with("1.  Item 1\n2.  Item 2\n"), "{out}");
        assert!(out.ends_with("10. Item 10\n"), "{out}");
        assert_eq!(
            text("<ol start=\"3\"><li>c</li><li>d</li></ol>", 20),
            "3. c\n4. d\n"
        );
        assert_eq!(
            text("<ol start=\"-1\"><li>a</li><li>b</li></ol>", 20),
            "-1. a\n0.  b\n"
        );
    }

    #[test]
    fn definition_lists() {
        assert_eq!(
            text("<dl><dt>Foo</dt><dd>Definition of foo</dd></dl>", 40),
            "*Foo*\n  Definition of foo\n"
        );
    }

    #[test]
    fn pre_is_verbatim_and_unwrapped() {
        assert_eq!(
            text("<pre>foo\nbar\nwib   asdf;\n</pre><p>Hello</p>", 20),
            "foo\nbar\nwib   asdf;\n\nHello\n"
        );
        // Long lines are kept rather than wrapped.
        let long = "x".repeat(60);
        assert_eq!(text(&format!("<pre>{long}</pre>"), 20), format!("{long}\n"));
        assert_eq!(text("<pre>a\tb</pre>", 40), "a       b\n");
    }

    // -- tables ------------------------------------------------------------

    #[test]
    fn table_is_drawn_with_rules() {
        assert_eq!(
            text("<table><tr><td>1</td><td>2</td><td>3</td></tr></table>", 12),
            "─┬─┬─\n1│2│3\n─┴─┴─\n"
        );
        assert_eq!(
            text(
                "<table><tr><td>1</td><td>2</td></tr><tr><td>3</td><td>4</td></tr></table>",
                12
            ),
            "─┬─\n1│2\n─┼─\n3│4\n─┴─\n"
        );
    }

    #[test]
    fn table_header_row_and_padding() {
        let html = "<table><thead><tr><th>Col1</th><th>Col2</th></tr></thead>\
                    <tbody><tr><td>1</td><td>2</td></tr></tbody></table>";
        assert_eq!(
            text(html, 15),
            "────┬────\nCol1│Col2\n────┼────\n1   │2   \n────┴────\n"
        );
    }

    #[test]
    fn table_cells_wrap_within_their_column() {
        let html = "<table><tr><td>Alpha beta gamma delta</td>\
                    <td>Second column text</td></tr></table>";
        let out = text(html, 24);
        for line in out.lines() {
            assert!(str_width(line) <= 24, "too wide: {line:?}");
        }
        assert!(out.contains('│'), "{out}");
        assert!(out.lines().count() >= 4, "{out}");
    }

    #[test]
    fn table_colspan_joins_columns() {
        let html = "<table><tr><td>1</td><td>2</td></tr>\
                    <tr><td colspan=\"2\">wide cell</td></tr></table>";
        let out = text(html, 20);
        assert!(out.contains("wide cell"), "{out}");
        for line in out.lines() {
            assert!(str_width(line) <= 20, "too wide: {line:?}");
        }
    }

    #[test]
    fn deeply_nested_tables_degrade_to_blocks() {
        let mut html = String::new();
        for _ in 0..12 {
            html.push_str("<table><tr><td>");
        }
        html.push_str("deep");
        for _ in 0..12 {
            html.push_str("</td></tr></table>");
        }
        let out = text(&html, 40);
        assert!(out.contains("deep"), "{out}");
        for line in out.lines() {
            assert!(str_width(line) <= 40, "too wide: {line:?}");
        }
    }

    #[test]
    fn table_with_absurd_column_count_falls_back() {
        let mut html = String::from("<table><tr>");
        for i in 0..200 {
            html.push_str(&format!("<td>c{i}</td>"));
        }
        html.push_str("</tr></table>");
        let out = text(&html, 40);
        assert!(out.contains("c0"), "{out}");
        assert!(!out.contains('│'), "{out}");
    }

    // -- inline ------------------------------------------------------------

    #[test]
    fn emphasis_uses_markdown_markers_in_plain_output() {
        assert_eq!(
            text("<p>Hi <em>em</em> <strong>strong</strong></p>", 30),
            "Hi *em* **strong**\n"
        );
        assert_eq!(text("<p>Hi <b>bold</b></p>", 30), "Hi **bold**\n");
        // html2text decorates <em> but not <i>.
        assert_eq!(text("<p><i>it</i></p>", 30), "it\n");
        assert_eq!(text("<p><code>fn()</code></p>", 30), "`fn()`\n");
    }

    #[test]
    fn strikeout_uses_combining_marks() {
        assert_eq!(
            text("Hi <s>you</s>thee!", 40),
            "Hi y\u{336}o\u{336}u\u{336}thee!\n"
        );
        assert_eq!(text("<del>x</del>", 40), "x\u{336}\n");
    }

    #[test]
    fn superscripts() {
        assert_eq!(text("x<sup>2</sup>", 80), "x²\n");
        assert_eq!(text("2<sup>32</sup>", 80), "2³²\n");
        assert_eq!(text("x<sup>ab</sup>", 80), "x^{ab}\n");
        // <sub> renders as ordinary text, as html2text does.
        assert_eq!(text("H<sub>2</sub>O", 80), "H2O\n");
    }

    #[test]
    fn images_use_their_alt_text() {
        assert_eq!(
            text("<p>Hello <img src='foo.jpg' alt='world'></p>", 80),
            "Hello [world]\n"
        );
        // Decorative images (no alt, or an empty one) are dropped, which
        // keeps tracking pixels and spacers out of the text.
        assert_eq!(text("<p>Hello x<img src='foo.jpg'>y</p>", 80), "Hello xy\n");
        assert_eq!(
            text("<p>Hello x<img src='foo.jpg' alt=''>y</p>", 80),
            "Hello xy\n"
        );
        assert_eq!(text("<p>Hello x<img alt='a'>y</p>", 80), "Hello xy\n");
    }

    // -- links -------------------------------------------------------------

    #[test]
    fn links_render_with_reference_footnotes() {
        assert_eq!(
            text(
                "<p>Hello, <a href=\"http://www.example.com/\">world</a></p>",
                80
            ),
            "Hello, [world][1]\n\n[1]: http://www.example.com/\n"
        );
        assert_eq!(
            text(
                "<p><a href=\"http://a/\">a</a> and <a href=\"http://b/\">b</a></p>",
                80
            ),
            "[a][1] and [b][2]\n\n[1]: http://a/\n[2]: http://b/\n"
        );
    }

    #[test]
    fn reference_lines_match_the_filter_td_news_uses() {
        let out = text("<p>See <a href=\"https://example.com/x\">this</a>.</p>", 40);
        let refs: Vec<&str> = out.lines().filter(|l| is_reference_link_def(l)).collect();
        assert_eq!(refs, vec!["[1]: https://example.com/x"]);
        let body: Vec<&str> = out
            .lines()
            .filter(|l| !is_reference_link_def(l) && !l.is_empty())
            .collect();
        assert_eq!(body, vec!["See [this][1]."]);
    }

    #[test]
    fn long_link_targets_wrap() {
        assert_eq!(
            text("<a href=\"http://www.example.com/\">Hello</a>", 10),
            "[Hello][1]\n\n[1]: http:\n//www.exam\nple.com/\n"
        );
    }

    #[test]
    fn empty_and_targetless_anchors_are_not_links() {
        assert_eq!(text("<a name=\"top\">Top</a>", 40), "Top\n");
        assert_eq!(text("<a href=\"http://x/\"></a>after", 40), "after\n");
    }

    // -- wrapping and widths ----------------------------------------------

    #[test]
    fn greedy_word_wrapping() {
        assert_eq!(text("Hello there boo", 20), "Hello there boo\n");
        assert_eq!(text("Hello there boo", 14), "Hello there\nboo\n");
        assert_eq!(text("Hello there boo", 10), "Hello\nthere boo\n");
        assert_eq!(text("Hello there boo", 5), "Hello\nthere\nboo\n");
        assert_eq!(text("Hello there boo", 4), "Hell\no\nther\ne\nboo\n");
        assert_eq!(
            text("Hello there boo", 1),
            "H\ne\nl\nl\no\nt\nh\ne\nr\ne\nb\no\no\n"
        );
    }

    #[test]
    fn overlong_words_are_split() {
        assert_eq!(
            text("<p>Hello, world.  Superlongwordreally</p>", 8),
            "Hello,\nworld.\nSuperlon\ngwordrea\nlly\n"
        );
        let word = "z".repeat(500);
        let out = text(&word, 40);
        assert_eq!(out.lines().count(), 13);
        for line in out.lines() {
            assert!(str_width(line) <= 40);
        }
    }

    #[test]
    fn markers_take_part_in_wrapping() {
        assert_eq!(text("Hello <em>there</em> boo", 20), "Hello *there* boo\n");
        assert_eq!(text("Hello <em>there</em> boo", 13), "Hello *there*\nboo\n");
        assert_eq!(text("Hello <em>there</em> boo", 12), "Hello\n*there* boo\n");
    }

    #[test]
    fn east_asian_characters_are_two_columns() {
        assert_eq!(char_width('日'), 2);
        assert_eq!(char_width('ｱ'), 1);
        assert_eq!(char_width('a'), 1);
        assert_eq!(char_width('\u{300}'), 0);
        assert_eq!(
            text("<p>日本語のテキストです</p>", 10),
            "日本語のテ\nキストです\n"
        );
    }

    #[test]
    fn nbsp_is_not_a_wrap_point() {
        assert_eq!(text("aaa&nbsp;bbb ccc ddd", 8), "aaa\u{a0}bbb\nccc ddd\n");
    }

    #[test]
    fn zero_width_of_zero_means_no_wrapping() {
        let long = "word ".repeat(40);
        let out = text(&long, 0);
        assert_eq!(out.lines().count(), 1);
        assert_eq!(str_width(out.trim_end()), 199);
    }

    #[test]
    fn output_has_no_trailing_blank_lines() {
        assert_eq!(text("<p>a</p><p></p><br><br>", 20), "a\n");
        assert_eq!(text("", 20), "");
        assert_eq!(text("   \n  ", 20), "");
        assert_eq!(text("<html><body></body></html>", 20), "");
    }

    // -- rich output -------------------------------------------------------

    #[test]
    fn rich_tags_nest_outermost_first() {
        let lines = to_rich(
            b"<span style=\"color: #ff0000\">red <a href=\"http://e/\">see <strong>this</strong></a></span>",
            40,
        );
        let spans: Vec<(&str, &[Tag])> = lines
            .first()
            .map(|l| {
                l.iter()
                    .map(|s| (s.text.as_str(), s.tags.as_slice()))
                    .collect()
            })
            .unwrap_or_default();
        let red = Rgb {
            r: 0xff,
            g: 0,
            b: 0,
        };
        assert_eq!(spans[0].0, "red ");
        assert_eq!(spans[0].1, [Tag::Colour(red)]);
        assert_eq!(spans[1].0, "see ");
        assert_eq!(
            spans[1].1,
            [Tag::Colour(red), Tag::Link("http://e/".to_string())]
        );
        assert_eq!(spans[2].0, "this");
        assert_eq!(
            spans[2].1,
            [
                Tag::Colour(red),
                Tag::Link("http://e/".to_string()),
                Tag::Strong
            ]
        );
    }

    #[test]
    fn rich_output_carries_tags_not_markers() {
        let lines = to_rich(b"<p><em>a</em> <strong>b</strong> <code>c</code></p>", 40);
        assert_eq!(flatten(&lines), "a b c\n");
        let tags: Vec<&[Tag]> = lines
            .first()
            .map(|l| l.iter().map(|s| s.tags.as_slice()).collect())
            .unwrap_or_default();
        assert_eq!(tags[0], [Tag::Emphasis]);
        assert_eq!(tags[2], [Tag::Strong]);
        assert_eq!(tags[4], [Tag::Code]);
    }

    #[test]
    fn rich_links_and_images_carry_their_target() {
        let lines = to_rich(
            b"<a href=\"http://e/x\">go</a> <img src=\"p.png\" alt=\"Pic\">",
            40,
        );
        let spans = lines.first().cloned().unwrap_or_default();
        assert_eq!(spans[0].text, "go");
        assert_eq!(spans[0].tags, [Tag::Link("http://e/x".to_string())]);
        assert_eq!(spans[2].text, "Pic");
        assert_eq!(spans[2].tags, [Tag::Image("p.png".to_string())]);
        // No footnote list in rich output: the caller has the target.
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn rich_preformat_and_strikeout() {
        let lines = to_rich(b"<pre>a  b</pre>", 40);
        assert_eq!(lines[0][0].tags, [Tag::Preformat]);
        assert_eq!(lines[0][0].text, "a  b");
        let lines = to_rich(b"<s>ab</s>", 40);
        assert_eq!(lines[0][0].tags, [Tag::Strikeout]);
    }

    #[test]
    fn colours_come_from_style_and_font_attributes() {
        let cases: [(&str, Rgb); 5] = [
            (
                "<span style=\"color:#f00\">x</span>",
                Rgb { r: 255, g: 0, b: 0 },
            ),
            (
                "<span style=\"color: #00ff00\">x</span>",
                Rgb { r: 0, g: 255, b: 0 },
            ),
            (
                "<span style=\"color: rgb(1, 2, 3)\">x</span>",
                Rgb { r: 1, g: 2, b: 3 },
            ),
            ("<font color=\"blue\">x</font>", Rgb { r: 0, g: 0, b: 255 }),
            (
                "<span style=\"color:teal\">x</span>",
                Rgb {
                    r: 0,
                    g: 128,
                    b: 128,
                },
            ),
        ];
        for (html, want) in cases {
            let lines = to_rich(html.as_bytes(), 40);
            assert_eq!(lines[0][0].tags, [Tag::Colour(want)], "{html}");
        }
        let lines = to_rich(b"<td bgcolor=\"#000000\">x</td>", 40);
        assert_eq!(lines[0][0].tags, [Tag::BgColour(Rgb { r: 0, g: 0, b: 0 })]);
        // Unparseable colours are simply absent.
        assert!(
            to_rich(b"<span style=\"color:chartreusish\">x</span>", 40)[0][0]
                .tags
                .is_empty()
        );
    }

    #[test]
    fn plain_is_the_flattened_rich_rendering_of_undecorated_content() {
        for html in [
            "<h1>Head</h1><p>Body text that needs wrapping to two lines</p>",
            "<ul><li>one</li><li>two</li></ul>",
            "<table><tr><td>a</td><td>b</td></tr></table>",
            "<blockquote>quoted</blockquote>",
            "<pre>verbatim</pre>",
        ] {
            assert_eq!(
                to_text(html.as_bytes(), 24),
                flatten(&to_rich(html.as_bytes(), 24)),
                "{html}"
            );
        }
    }

    // -- corpus ------------------------------------------------------------

    #[test]
    fn plain_mail_body() {
        let html = "<html><body><div>Hi Sam,</div><div><br></div>\
                    <div>Can you take a look at the deploy notes before Friday?</div>\
                    <div><br></div><div>Thanks,<br>Jo</div></body></html>";
        assert_eq!(
            text(html, 40),
            "Hi Sam,\n\nCan you take a look at the deploy notes\nbefore Friday?\n\nThanks,\nJo\n"
        );
    }

    #[test]
    fn marketing_newsletter() {
        let html = "<body bgcolor=\"#ffffff\"><table width=\"100%\"><tr><td align=\"center\">\
             <img src=\"https://cdn.example.com/logo.png\" alt=\"ACME\"></td></tr>\
             <tr><td><h1 style=\"color:#333\">Deals</h1>\
             <p>This week&rsquo;s picks &mdash; hand&nbsp;picked.</p>\
             <table><thead><tr><th>Item</th><th>Now</th></tr></thead>\
             <tbody><tr><td>Cordless drill</td><td><b>&pound;59.99</b></td></tr></tbody></table>\
             <p><a href=\"https://example.com/shop\">Shop now</a></p>\
             <img src=\"https://track.example.com/px.gif\" alt=\"\"></td></tr></table></body>";
        let out = text(html, 60);
        for line in out.lines() {
            assert!(str_width(line) <= 60, "too wide: {line:?}");
        }
        assert!(out.contains("[ACME]"), "{out}");
        assert!(out.contains("# Deals"), "{out}");
        assert!(
            out.contains("This week’s picks — hand\u{a0}picked."),
            "{out}"
        );
        assert!(out.contains("Item"), "{out}");
        assert!(out.contains("**£59.99**"), "{out}");
        assert!(out.contains("[Shop now][1]"), "{out}");
        assert!(out.contains("[1]: https://example.com/shop"), "{out}");
        // The tracking pixel leaves no trace.
        assert!(!out.contains("px.gif"), "{out}");
    }

    #[test]
    fn feed_article() {
        let html = "<div class=\"entry\"><h2>Rewriting the renderer</h2>\
             <p>We replaced it. The <em>old</em> one pulled in <strong>fourteen</strong> crates.</p>\
             <h3>What changed</h3><ul><li>No dependencies</li><li>Bounded nesting\
             <ul><li>depth capped</li></ul></li></ul>\
             <blockquote><p>It should look the same.</p></blockquote>\
             <ol><li>3.2&nbsp;MB smaller</li><li>18&times; faster</li></ol>\
             <pre>fn main() {}</pre>\
             <p>Read the <a href=\"https://example.org/post\">full post</a>.</p></div>";
        assert_eq!(
            text(html, 60),
            "## Rewriting the renderer\n\
             \n\
             We replaced it. The *old* one pulled in **fourteen** crates.\n\
             \n\
             ### What changed\n\
             * No dependencies\n\
             * Bounded nesting\n\
             \u{20} * depth capped\n\
             \n\
             > It should look the same.\n\
             1. 3.2\u{a0}MB smaller\n\
             2. 18× faster\n\
             \n\
             fn main() {}\n\
             \n\
             Read the [full post][1].\n\
             \n\
             [1]: https://example.org/post\n"
        );
    }

    // -- bounds ------------------------------------------------------------

    #[test]
    fn pathological_nesting_is_bounded() {
        let depth = 20_000;
        let mut html = String::with_capacity(depth * 11 + 16);
        for _ in 0..depth {
            html.push_str("<div>");
        }
        html.push_str("deep text");
        for _ in 0..depth {
            html.push_str("</div>");
        }
        assert!(html.len() > 200_000);
        let start = std::time::Instant::now();
        let out = text(&html, 40);
        assert_eq!(out, "deep text\n");
        assert!(start.elapsed().as_secs() < 5, "took {:?}", start.elapsed());
    }

    #[test]
    fn pathological_unclosed_nesting_is_bounded() {
        let mut html = String::new();
        for i in 0..20_000 {
            html.push_str(if i % 2 == 0 { "<div><b>" } else { "<ul><li>" });
        }
        html.push_str("end");
        let out = text(&html, 40);
        assert!(out.contains("end"), "{out}");
    }

    #[test]
    fn long_flat_documents_stay_linear() {
        let mut html = String::new();
        for i in 0..20_000 {
            html.push_str(&format!("<p>Paragraph number {i} with some words.</p>"));
        }
        let start = std::time::Instant::now();
        let out = text(&html, 40);
        assert!(out.lines().count() > 20_000);
        assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    }

    #[test]
    fn deeply_nested_lists_stay_within_the_width() {
        let mut html = String::new();
        for _ in 0..200 {
            html.push_str("<ul><li>");
        }
        html.push_str("item");
        for _ in 0..200 {
            html.push_str("</li></ul>");
        }
        let out = text(&html, 20);
        for line in out.lines() {
            assert!(str_width(line) <= 20, "too wide: {line:?}");
        }
        // Indentation stops once it would eat the whole width; the item text
        // is still there, wrapped into the columns that remain.
        assert!(out.starts_with("* * * "), "{out}");
        assert!(
            out.replace(['*', ' ', '\n'], "").starts_with("item"),
            "{out}"
        );
    }

    #[test]
    fn urls_survive_the_wide_scavenging_pass() {
        // td-news renders at width 200 purely to collect URLs from the text.
        let html = "<p><a href=\"https://example.com/one\">a</a> \
                    <a href=\"https://example.com/two?q=1&amp;r=2\">b</a></p>";
        let out = text(html, 200);
        assert!(out.contains("[1]: https://example.com/one"), "{out}");
        assert!(
            out.contains("[2]: https://example.com/two?q=1&r=2"),
            "{out}"
        );
        assert_eq!(out.lines().filter(|l| is_reference_link_def(l)).count(), 2);
    }

    #[test]
    fn unclosed_opaque_elements_do_not_swallow_the_page() {
        // Script and friends are opaque, so an unclosed one eats the rest,
        // exactly as a browser would.
        assert_eq!(text("<p>a</p><script>x()", 40), "a\n");
        // Anything that could hold readable text stays a container instead.
        assert_eq!(
            text("<p>a</p><object>fallback<p>b</p>", 40),
            "a\n\nfallback\n\nb\n"
        );
        assert_eq!(
            text("<p>a</p><video>b</video><p>c</p>", 40),
            "a\n\nb\n\nc\n"
        );
    }

    #[test]
    fn zero_width_break_hints_are_invisible() {
        assert_eq!(text("long<wbr>word", 40), "longword\n");
        assert_eq!(text("a\u{200b}b", 40), "a\u{200b}b\n");
        assert_eq!(str_width("a\u{200b}b"), 2);
    }

    #[test]
    fn adversarial_shapes_stay_linear() {
        let cases = [
            // Unclosed opens followed by unmatched closes: end-tag matching
            // must not walk an unbounded stack.
            "<div>".repeat(20_000) + &"</span>".repeat(20_000),
            // Many blank lines before any content, then many blocks.
            "<br>".repeat(20_000) + &"<p>x</p>".repeat(20_000),
            // A wide, tall table.
            format!(
                "<table>{}</table>",
                "<tr><td>cell</td><td>cell</td><td>cell</td></tr>".repeat(5_000)
            ),
            // One enormous word.
            "z".repeat(200_000),
            // Deeply nested inline markup.
            "<b><i><code>".repeat(5_000) + "x",
        ];
        for html in cases {
            let start = std::time::Instant::now();
            let out = text(&html, 60);
            assert!(
                start.elapsed().as_secs() < 10,
                "{:?} took {:?}",
                html.get(..20),
                start.elapsed()
            );
            let _ = out;
        }
    }
}
