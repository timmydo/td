//! A page's document tree (td-web/DESIGN.md §10, "Pipeline and threads" and
//! "Places"): td-html's tree, parsed under the page's tree term, with
//! td-web's tables over it, each a node's: its place in tree order and the
//! link it lies in. The markup td-web writes for its own pages and for a
//! text page is made here too.

use td_alloc::{Budget, BudgetError, BudgetShared, BudgetString, BudgetVec};
use td_html::document::{parse_with_budget, Document, Namespace, NodeId};
use td_html::html5::{ParseError, ParseLimits, MAX_TREE_BYTES};

/// A node with no rank: outside the tree, as a template's contents are.
pub const NONE: u32 = u32::MAX;
/// The most characters of a page's title.
pub const MAX_TITLE: usize = 200;
/// What a text page's text follows: td-html's tokenizer ends markup at
/// `plaintext`, so the rest is one text node.
pub const TEXT_PREFIX: &str = "<!doctype html><plaintext>";
/// The most bytes of text td-web writes a page of its own from.
pub const MAX_OWN_TEXT: usize = 4096;

/// What a page was made from, which decides what `F9` and a relayout do.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    /// A reply's markup.
    Markup,
    /// A text page (td-web/DESIGN.md §10, "Cuts"), a reply's text or rung 3's.
    Text,
    /// A page td-web wrote: an error, or a type it does not show.
    Own,
}

pub struct Tree {
    document: BudgetShared<Document>,
    source: Source,
    /// Each node's place in tree order, by its index.
    rank: BudgetVec<u32>,
    /// Each node's nearest `a` or `area` with an `href`, itself included.
    link: BudgetVec<Option<NodeId>>,
    /// `style` elements whose type is CSS's and `<link rel=stylesheet>`s
    /// with an `href`, in tree order.
    sheets: BudgetVec<NodeId>,
    /// `img` elements with a `src` not blank or a `srcset`, in tree
    /// order: the first `MAX_PICTURES` of them, and how many more there
    /// are.
    pictures: BudgetVec<NodeId>,
    past: usize,
    base: Option<NodeId>,
    title: BudgetString,
}

/// The name of the td-html limit a refusal met, for the status row.
pub fn refused(error: ParseError) -> &'static str {
    match error {
        ParseError::Input => "MAX_INPUT_BYTES",
        ParseError::Nodes => "MAX_NODES",
        ParseError::Attributes => "MAX_ATTRIBUTES",
        ParseError::Bytes => "MAX_TREE_BYTES",
        ParseError::Depth => "MAX_DEPTH",
        ParseError::AttributesPerTag => "MAX_ATTRIBUTES_PER_TAG",
        ParseError::NameBytes => "MAX_NAME_BYTES",
        ParseError::AttributeBytes => "MAX_ATTRIBUTE_BYTES",
        ParseError::Work => "MAX_WORK",
        ParseError::State => "the parser's state",
        ParseError::Allocation => "memory",
    }
}

/// Why a tree was not made.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refusal {
    Parse(&'static str),
    Memory,
}
impl From<BudgetError> for Refusal {
    fn from(_: BudgetError) -> Self {
        Self::Memory
    }
}

fn html_element(document: &Document, id: NodeId) -> Option<&td_html::document::Element> {
    document
        .node(id)?
        .element()
        .filter(|element| element.namespace == Namespace::Html)
}

/// Whether a `style` element's `type`, if any, is CSS's: empty or
/// `text/css` itself (HTML, "update a style block").
fn css(element: &td_html::document::Element) -> bool {
    let kind = element.attribute("type").unwrap_or("");
    kind.is_empty() || kind.trim().eq_ignore_ascii_case("text/css")
}

/// Whether a `link` names a style sheet to apply: its `rel` holds
/// `stylesheet` and not `alternate`, it is not `disabled`, it has an
/// `href`, and its `type`, if any, is CSS's by its MIME essence
/// (td-web/DESIGN.md §10, "Subresources").
fn linked(element: &td_html::document::Element) -> bool {
    let rel = element.attribute("rel").unwrap_or("");
    let has = |name: &str| {
        rel.split(|c: char| c.is_ascii_whitespace())
            .any(|token| token.eq_ignore_ascii_case(name))
    };
    let kind = element.attribute("type").unwrap_or("");
    let essence = kind.split(';').next().unwrap_or("").trim();
    has("stylesheet")
        && !has("alternate")
        && element.attribute("disabled").is_none()
        && (kind.trim().is_empty() || essence.eq_ignore_ascii_case("text/css"))
        && element
            .attribute("href")
            .is_some_and(|href| !href.trim().is_empty())
}

/// Whether an `img` names a picture to fetch: a `src` that is not blank,
/// or a `srcset` (td-web/DESIGN.md §10, "Subresources"). Which of them
/// it shows is `sources::select`'s; td-mail reads its `src` alone.
fn picture(element: &td_html::document::Element) -> bool {
    element.attribute("src").is_some_and(|src| {
        !src.trim_matches(|c: char| c.is_ascii_whitespace())
            .is_empty()
    }) || srcset(element).is_some()
}

/// An `img`'s `srcset`: one not blank and within `MAX_SRCSET_BYTES`, a
/// longer one counted absent.
fn srcset(element: &td_html::document::Element) -> Option<&str> {
    element.attribute("srcset").filter(|srcset| {
        srcset.len() <= crate::sources::MAX_SRCSET_BYTES
            && !srcset
                .trim_matches(|c: char| c.is_ascii_whitespace())
                .is_empty()
    })
}

/// Whether a sheet is in the preferred style sheet set: untitled, or
/// titled as the first titled sheet was, which names the set.
fn in_set<'a>(element: &'a td_html::document::Element, preferred: &mut Option<&'a str>) -> bool {
    match element.attribute("title").filter(|title| !title.is_empty()) {
        None => true,
        Some(title) => *preferred.get_or_insert(title) == title,
    }
}

/// A page's own sheet as its tree gives it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sheet<'a> {
    /// A `style` element's text and `media`.
    Inline {
        text: &'a str,
        media: Option<&'a str>,
    },
    /// A `<link rel=stylesheet>`'s `href` and `media`.
    Linked {
        href: &'a str,
        media: Option<&'a str>,
    },
}

/// The next node after `id` in tree order, its subtree included unless
/// `skip` is set; none past the last.
fn next(document: &Document, id: NodeId, skip: bool) -> Option<NodeId> {
    let node = document.node(id)?;
    if !skip {
        if let Some(child) = node.first_child() {
            return Some(child);
        }
    }
    let mut at = Some(id);
    while let Some(here) = at {
        let node = document.node(here)?;
        if let Some(sibling) = node.next_sibling() {
            return Some(sibling);
        }
        at = node.parent();
    }
    None
}

impl Tree {
    /// `markup` parsed under a child of `page` capped at td-html's tree
    /// ceiling, its tables charged to `page`.
    pub fn parse(markup: &str, source: Source, page: &Budget) -> Result<Self, Refusal> {
        let tree = page.child(MAX_TREE_BYTES.saturating_add(Budget::metadata_bytes()))?;
        let document = parse_with_budget(markup, ParseLimits::default(), &tree).map_err(
            |error| match error {
                ParseError::Allocation => Refusal::Memory,
                error => Refusal::Parse(refused(error)),
            },
        )?;
        let document = BudgetShared::new(page, document)?;
        Self::over(document, source, page)
    }

    fn over(
        document: BudgetShared<Document>,
        source: Source,
        page: &Budget,
    ) -> Result<Self, Refusal> {
        let count = document.len();
        let mut rank = BudgetVec::filled(page, count, NONE)?;
        let mut link = BudgetVec::filled(page, count, None)?;
        let mut sheets = BudgetVec::new(page)?;
        let mut pictures = BudgetVec::new(page)?;
        let mut past = 0usize;
        let mut base = None;
        let mut title = None;
        // The preferred style sheet set's name: the first titled sheet's
        // (CSSOM, "create a CSS style sheet").
        let mut preferred: Option<&str> = None;
        let mut at = Some(document.root());
        let mut order = 0u32;
        while let Some(id) = at {
            if let Some(slot) = rank.get_mut(id.index()) {
                *slot = order;
            }
            order = order.saturating_add(1);
            let node = document.node(id);
            let inherited = node
                .and_then(|node| node.parent())
                .and_then(|parent| link.get(parent.index()).copied())
                .flatten();
            let mut own = inherited;
            if let Some(element) = html_element(&document, id) {
                let name = element.name.as_str();
                let href = element.attribute("href");
                match name {
                    "a" | "area" if href.is_some() => own = Some(id),
                    "base" if href.is_some() && base.is_none() => base = Some(id),
                    "title" if title.is_none() => title = Some(id),
                    "style" if css(element) && in_set(element, &mut preferred) => {
                        sheets.try_push(id)?
                    }
                    "link" if linked(element) && in_set(element, &mut preferred) => {
                        sheets.try_push(id)?
                    }
                    "img" if picture(element) => {
                        if pictures.len() < crate::inline::MAX_PICTURES {
                            pictures.try_push(id)?;
                        } else {
                            past = past.saturating_add(1);
                        }
                    }
                    _ => {}
                }
            }
            if let Some(slot) = link.get_mut(id.index()) {
                *slot = own;
            }
            at = next(&document, id, false);
        }
        let mut text = BudgetString::new(page)?;
        if let Some(id) = title {
            collapsed(&document, id, &mut text)?;
        }
        Ok(Self {
            document,
            source,
            rank,
            link,
            sheets,
            pictures,
            past,
            base,
            title: text,
        })
    }

    pub fn document(&self) -> &Document {
        &self.document
    }

    /// A handle on the document for a styled document to hold.
    pub fn shared(&self) -> Result<BudgetShared<Document>, BudgetError> {
        self.document.try_clone()
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// `id`'s place in tree order; `NONE` outside the tree.
    pub fn rank(&self, id: NodeId) -> u32 {
        self.rank.get(id.index()).copied().unwrap_or(NONE)
    }

    /// The last node of `id`'s subtree in tree order, `id` itself for a
    /// leaf: an inline-block's places end past its places (text.rs).
    pub fn last_within(&self, id: NodeId) -> NodeId {
        let mut last = id;
        while let Some(child) = self.document.node(last).and_then(|n| n.last_child()) {
            last = child;
        }
        last
    }

    /// The link `id` lies in, itself included.
    pub fn link(&self, id: NodeId) -> Option<NodeId> {
        self.link.get(id.index()).copied().flatten()
    }

    /// A link's `href`, as written.
    pub fn href(&self, link: NodeId) -> Option<&str> {
        html_element(&self.document, link)?.attribute("href")
    }

    /// An element's `alt` text: an image's or an area's.
    pub fn alt(&self, id: NodeId) -> Option<&str> {
        html_element(&self.document, id)?.attribute("alt")
    }

    /// The first `<base href>`, as written.
    pub fn base(&self) -> Option<&str> {
        html_element(&self.document, self.base?)?.attribute("href")
    }

    /// The first `title` element's text, its spaces collapsed.
    pub fn title(&self) -> &str {
        self.title.as_str()
    }

    /// The `style` elements whose type is CSS's and the style sheet
    /// `link`s, in tree order.
    pub fn sheets(&self) -> &[NodeId] {
        self.sheets.as_slice()
    }

    /// The `img` elements with a `src` or a `srcset` fetched, in tree
    /// order, at most `MAX_PICTURES`.
    pub fn pictures(&self) -> &[NodeId] {
        self.pictures.as_slice()
    }

    /// The `img` elements with a `src` or a `srcset` past `MAX_PICTURES`.
    pub fn past(&self) -> usize {
        self.past
    }

    /// An `img`'s `src`, as written, its ASCII white space trimmed: never
    /// a `srcset` candidate, which td-mail does not take.
    pub fn src(&self, id: NodeId) -> Option<&str> {
        let src = html_element(&self.document, id)?.attribute("src")?;
        Some(src.trim_matches(|c: char| c.is_ascii_whitespace()))
    }

    /// An `img`'s `srcset`, as written: none blank or past
    /// `MAX_SRCSET_BYTES`.
    pub fn srcset(&self, id: NodeId) -> Option<&str> {
        srcset(html_element(&self.document, id)?)
    }

    /// An `img`'s `sizes`, as written.
    pub fn sizes(&self, id: NodeId) -> Option<&str> {
        html_element(&self.document, id)?.attribute("sizes")
    }

    /// The URL a chosen candidate names, as written.
    pub fn candidate(&self, chosen: crate::sources::Chosen) -> Option<&str> {
        match chosen.attribute {
            crate::sources::Attribute::Src => self.src(chosen.holder),
            crate::sources::Attribute::Srcset => {
                let (start, end) = chosen.span;
                let srcset = self.srcset(chosen.holder)?;
                srcset.get(usize::try_from(start).ok()?..usize::try_from(end).ok()?)
            }
        }
    }

    /// A `style` element's text, or a `link`'s `href`, with its `media`.
    pub fn sheet(&self, id: NodeId) -> Option<Sheet<'_>> {
        let element = html_element(&self.document, id)?;
        let media = element.attribute("media");
        if element.name.as_str() == "link" {
            let href = element.attribute("href")?;
            return Some(Sheet::Linked { href, media });
        }
        let text = self
            .document
            .children(id)
            .find_map(|(_, child)| child.text())
            .unwrap_or("");
        Some(Sheet::Inline { text, media })
    }

    /// An element's `media` attribute.
    pub fn media(&self, id: NodeId) -> Option<&str> {
        html_element(&self.document, id)?.attribute("media")
    }

    /// The element a URL's fragment names: the first whose `id` it is,
    /// else the first `a` whose `name` it is.
    pub fn target(&self, fragment: &str) -> Option<NodeId> {
        if fragment.is_empty() {
            return None;
        }
        let mut named = None;
        let mut at = Some(self.document.root());
        while let Some(id) = at {
            if let Some(element) = html_element(&self.document, id) {
                if element.attribute("id") == Some(fragment) {
                    return Some(id);
                }
                if named.is_none()
                    && element.name.as_str() == "a"
                    && element.attribute("name") == Some(fragment)
                {
                    named = Some(id);
                }
            }
            at = next(&self.document, id, false);
        }
        named
    }

    /// The node after `id` in tree order, its subtree included unless
    /// `skip` is set.
    pub fn next(&self, id: NodeId, skip: bool) -> Option<NodeId> {
        next(&self.document, id, skip)
    }
}

/// `id`'s text, each run of white space one space, none at either end,
/// at most `MAX_TITLE` characters.
fn collapsed(document: &Document, id: NodeId, out: &mut BudgetString) -> Result<(), BudgetError> {
    let mut count = 0usize;
    let mut space = false;
    for (_, child) in document.children(id) {
        let Some(text) = child.text() else {
            continue;
        };
        for c in text.chars() {
            if c.is_whitespace() {
                space = count > 0;
                continue;
            }
            if c.is_control() {
                continue;
            }
            if count >= MAX_TITLE {
                return Ok(());
            }
            if space {
                if count + 1 >= MAX_TITLE {
                    return Ok(());
                }
                out.try_push(' ')?;
                count += 1;
                space = false;
            }
            out.try_push(c)?;
            count += 1;
        }
    }
    Ok(())
}

/// What td-html makes of a scalar of a text page: a control but the tab
/// and the newline dropped, CR LF and a lone CR made LF.
fn kept(c: char) -> bool {
    !c.is_control() || c == '\t' || c == '\n'
}

/// `text` as a text page's text node holds it, at most `most` bytes, on
/// a scalar boundary.
pub fn text_node(text: &str, most: usize, memory: &Budget) -> Result<BudgetString, BudgetError> {
    let mut out = td_alloc::BudgetString::with_capacity(memory, text.len().min(most))?;
    let mut scalars = text.chars().peekable();
    while let Some(c) = scalars.next() {
        let c = if c == '\r' {
            if scalars.peek() == Some(&'\n') {
                continue;
            }
            '\n'
        } else {
            c
        };
        if !kept(c) {
            continue;
        }
        if out.len().saturating_add(c.len_utf8()) > most {
            break;
        }
        out.try_push(c)?;
    }
    Ok(out)
}

/// A text page's markup: the prefix, then `text` as its node holds it,
/// the two within `most` bytes. Returns the markup and where the text
/// starts in it.
pub fn text_markup(text: &str, most: usize, memory: &Budget) -> Result<BudgetString, BudgetError> {
    let room = most.saturating_sub(TEXT_PREFIX.len());
    let node = text_node(text, room, memory)?;
    let mut out = td_alloc::BudgetString::with_capacity(memory, TEXT_PREFIX.len() + node.len())?;
    out.try_push_str(TEXT_PREFIX)?;
    out.try_push_str(node.as_str())?;
    Ok(out)
}

/// A page td-web writes, its text escaped: each paragraph of `text`,
/// parted by a blank line, a `p`, its lines parted by `br`; at most
/// `MAX_OWN_TEXT` bytes of it.
pub fn own_markup(text: &str, memory: &Budget) -> Result<BudgetString, BudgetError> {
    let text = text
        .get(..text.floor_char_boundary(MAX_OWN_TEXT))
        .unwrap_or("");
    let mut out = td_alloc::BudgetString::with_capacity(memory, 64 + text.len() * 2)?;
    out.try_push_str("<!doctype html><body><p>")?;
    let mut newlines = 0usize;
    for c in text.chars() {
        if c == '\n' {
            newlines += 1;
            continue;
        }
        match newlines {
            0 => {}
            1 => out.try_push_str("<br>")?,
            _ => out.try_push_str("</p><p>")?,
        }
        newlines = 0;
        match c {
            '&' => out.try_push_str("&amp;")?,
            '<' => out.try_push_str("&lt;")?,
            '>' => out.try_push_str("&gt;")?,
            c if c.is_control() && c != '\t' => {}
            c => out.try_push(c)?,
        }
    }
    out.try_push_str("</p>")?;
    Ok(out)
}

/// Whether `id` is a text node. For tests.
pub fn is_text(document: &Document, id: NodeId) -> bool {
    use td_html::document::NodeKind;
    matches!(
        document.node(id).map(|node| node.kind()),
        Some(NodeKind::Text(_))
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn tree(markup: &str) -> Tree {
        let page = Budget::new(64 << 20).unwrap();
        Tree::parse(markup, Source::Markup, &page).unwrap()
    }

    fn named(tree: &Tree, id: NodeId) -> &str {
        tree.document()
            .node(id)
            .and_then(|node| node.element())
            .map_or("#text", |element| element.name.as_str())
    }

    #[test]
    fn ranks_follow_tree_order_and_links_reach_their_descendants() {
        let tree = tree(
            "<title> A \n  title\t</title><base href=/x/><base href=/y/>\
             <p>a<a href=b><i>c</i></a><a name=n>d</a><a href=e id=f>g</a>",
        );
        assert_eq!(tree.title(), "A title");
        assert_eq!(tree.base(), Some("/x/"));
        let document = tree.document();
        let mut last = None;
        let mut at = Some(document.root());
        while let Some(id) = at {
            let rank = tree.rank(id);
            assert!(last.is_none_or(|last| rank > last));
            last = Some(rank);
            at = tree.next(id, false);
        }
        let italic = {
            let mut at = Some(document.root());
            while at.is_some_and(|id| named(&tree, id) != "i") {
                at = at.and_then(|id| tree.next(id, false));
            }
            at.unwrap()
        };
        let link = tree.link(italic).unwrap();
        assert_eq!(tree.href(link), Some("b"));
        let anchor = tree.target("n").unwrap();
        assert!(tree.link(anchor).is_none(), "a name is no link");
        assert_eq!(tree.href(tree.target("f").unwrap()), Some("e"));
        assert!(tree.target("missing").is_none());
    }

    #[test]
    fn only_css_style_elements_and_style_sheet_links_are_sheets() {
        let tree = tree(
            "<style>a{}</style><link rel='Preload StyleSheet' href=x.css media=screen>\
             <style type=TEXT/CSS media=print>b{}</style>\
             <link rel='alternate stylesheet' href=alt.css><link rel=stylesheet>\
             <link rel=stylesheet href=' '><link rel=icon href=i.png>\
             <link rel=stylesheet type=text/less href=l.less>\
             <style type=text/less>c{}</style><template><style>d{}</style></template>\
             <link rel=stylesheet href=late.css>",
        );
        let sheets: Vec<_> = tree
            .sheets()
            .iter()
            .map(|&id| tree.sheet(id).unwrap())
            .collect();
        assert_eq!(
            sheets,
            [
                Sheet::Inline {
                    text: "a{}",
                    media: None
                },
                Sheet::Linked {
                    href: "x.css",
                    media: Some("screen")
                },
                Sheet::Inline {
                    text: "b{}",
                    media: Some("print")
                },
                Sheet::Linked {
                    href: "late.css",
                    media: None
                },
            ]
        );
    }

    #[test]
    fn disabled_links_are_left_out_and_only_the_preferred_set_applies() {
        let tree = tree(
            "<link rel=stylesheet href=off.css disabled>\
             <link rel=stylesheet href=css.css type='Text/CSS; charset=utf-8'>\
             <link rel=stylesheet href=u.css title=''>\
             <link rel=stylesheet href=one.css title=One>\
             <style title=Two>two{}</style>\
             <link rel=stylesheet href=two.css title=Two>\
             <style title=One>one{}</style><style>plain{}</style>\
             <style type='text/css; charset=utf-8'>not{}</style>",
        );
        let sheets: Vec<_> = tree
            .sheets()
            .iter()
            .map(|&id| match tree.sheet(id).unwrap() {
                Sheet::Inline { text, .. } => text,
                Sheet::Linked { href, .. } => href,
            })
            .collect();
        // A `link`'s type is read by its essence, a `style`'s whole; a
        // titled sheet applies only in the first title's set.
        assert_eq!(sheets, ["css.css", "u.css", "one.css", "one{}", "plain{}"]);
    }

    #[test]
    fn a_text_page_is_one_text_node_as_td_web_writes_it() {
        let page = Budget::new(64 << 20).unwrap();
        let text = "a\r\nb\rc\u{0}\u{7}\td <p>&amp;";
        let markup = text_markup(text, 1 << 20, &page).unwrap();
        let tree = Tree::parse(&markup, Source::Text, &page).unwrap();
        let document = tree.document();
        let node = (0..)
            .scan(Some(document.root()), |at, _| {
                let id = (*at)?;
                *at = tree.next(id, false);
                Some(id)
            })
            .find(|&id| is_text(document, id))
            .unwrap();
        assert_eq!(
            document.node(node).unwrap().text(),
            Some("a\nb\nc\td <p>&amp;")
        );
        let cut = text_markup("ééé", TEXT_PREFIX.len() + 5, &page).unwrap();
        assert_eq!(&cut[TEXT_PREFIX.len()..], "éé");
    }

    #[test]
    fn own_pages_escape_their_text() {
        let page = Budget::new(1 << 20).unwrap();
        let markup = own_markup("<b>&\nsecond\n\nnext", &page).unwrap();
        assert_eq!(
            markup.as_str(),
            "<!doctype html><body><p>&lt;b&gt;&amp;<br>second</p><p>next</p>"
        );
    }

    #[test]
    fn a_refused_parse_names_its_limit_and_returns_its_charge() {
        let page = Budget::new(1 << 20).unwrap();
        let before = page.used();
        let deep = "<div>".repeat(300);
        assert_eq!(
            Tree::parse(&deep, Source::Markup, &page).err(),
            Some(Refusal::Parse("MAX_DEPTH"))
        );
        assert_eq!(page.used(), before);
    }
}
