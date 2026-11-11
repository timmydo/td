use super::builder::Builder;
use super::*;
use crate::html5::Tag;
impl Builder<'_> {
    pub(super) fn convert_attribute(
        &mut self,
        a: &html5::Attribute,
        ns: Namespace,
    ) -> Result<Attribute, ParseError> {
        self.tok.budget.attribute()?;
        let (prefix, name, namespace) = if ns != Namespace::Html {
            match a.name.as_str() {
                "xlink:actuate" | "xlink:arcrole" | "xlink:href" | "xlink:role" | "xlink:show"
                | "xlink:title" | "xlink:type" => (
                    Some("xlink"),
                    a.name.get(6..).unwrap_or_default(),
                    AttributeNamespace::XLink,
                ),
                "xml:lang" | "xml:space" => (
                    Some("xml"),
                    a.name.get(4..).unwrap_or_default(),
                    AttributeNamespace::Xml,
                ),
                "xmlns" => (None, "xmlns", AttributeNamespace::Xmlns),
                "xmlns:xlink" => (Some("xmlns"), "xlink", AttributeNamespace::Xmlns),
                other => (None, attribute_name(other, ns), AttributeNamespace::None),
            }
        } else {
            (None, a.name.as_str(), AttributeNamespace::None)
        };
        Ok(Attribute {
            name: self.owned(name)?,
            prefix: prefix.map(|p| match p {
                "xml" => AttributePrefix::Xml,
                "xmlns" => AttributePrefix::Xmlns,
                _ => AttributePrefix::XLink,
            }),
            namespace,
            value: self.owned(&a.value)?,
        })
    }
    pub(super) fn foreign_text(&self) -> bool {
        self.current_element()
            .is_some_and(|e| e.namespace != Namespace::Html && !integration(e) && !math_text(e))
    }
    pub(super) fn foreign_token(&mut self, token: &Token) -> Result<bool, ParseError> {
        let Some(e) = self.current_element() else {
            return Ok(false);
        };
        if e.namespace == Namespace::Html {
            return Ok(false);
        }
        if let Token::Tag(t) = token {
            if !t.end
                && (integration(e)
                    || (math_text(e) && !matches!(t.name.as_str(), "mglyph" | "malignmark"))
                    || (e.namespace == Namespace::MathMl
                        && e.name == "annotation-xml"
                        && t.name == "svg"))
            {
                return Ok(false);
            }
        }
        match token {
            Token::Comment(s) => self.comment(s, self.current())?,
            Token::Doctype(_) => self.error(),
            Token::Tag(t)
                if (!t.end && breakout(t)) || (t.end && matches!(t.name.as_str(), "br" | "p")) =>
            {
                loop {
                    self.step()?;
                    if self.current_element().is_none_or(|e| {
                        e.namespace == Namespace::Html || integration(e) || math_text(e)
                    }) {
                        break;
                    }
                    self.open.pop();
                }
                return Ok(false);
            }
            Token::Tag(t) if !t.end => {
                let ns = self
                    .current_element()
                    .map(|e| e.namespace)
                    .unwrap_or(Namespace::Html);
                self.insert(t, ns, !t.self_closing)?;
            }
            Token::Tag(t) => {
                for i in (0..self.open.len()).rev() {
                    self.step()?;
                    let id = *self.open.get(i).ok_or(ParseError::State)?;
                    let Some(e) = self.node(id)?.element() else {
                        continue;
                    };
                    if e.namespace == Namespace::Html {
                        return Ok(false);
                    }
                    if e.name.eq_ignore_ascii_case(&t.name) {
                        self.open.truncate(i);
                        return Ok(true);
                    }
                    if e.namespace == Namespace::Html {
                        return Ok(false);
                    }
                }
            }
            Token::Text(_) => return Err(ParseError::State),
        }
        Ok(true)
    }
}
pub(super) fn math_text(e: &Element) -> bool {
    e.namespace == Namespace::MathMl
        && matches!(e.name.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
}
pub(super) fn integration(e: &Element) -> bool {
    e.html_integration
}
pub(super) fn scope_boundary(e: &Element) -> bool {
    match e.namespace {
        Namespace::Html => matches!(
            e.name.as_str(),
            "applet"
                | "caption"
                | "html"
                | "table"
                | "td"
                | "th"
                | "marquee"
                | "object"
                | "template"
        ),
        Namespace::Svg => matches!(e.name.as_str(), "foreignObject" | "desc" | "title"),
        Namespace::MathMl => math_text(e) || e.name == "annotation-xml",
    }
}
pub(super) fn special(e: &Element) -> bool {
    match e.namespace {
        Namespace::Svg => matches!(e.name.as_str(), "foreignObject" | "desc" | "title"),
        Namespace::MathMl => math_text(e) || e.name == "annotation-xml",
        Namespace::Html => matches!(
            e.name.as_str(),
            "address"
                | "applet"
                | "area"
                | "article"
                | "aside"
                | "base"
                | "basefont"
                | "bgsound"
                | "blockquote"
                | "body"
                | "br"
                | "button"
                | "caption"
                | "center"
                | "col"
                | "colgroup"
                | "dd"
                | "details"
                | "dir"
                | "div"
                | "dl"
                | "dt"
                | "embed"
                | "fieldset"
                | "figcaption"
                | "figure"
                | "footer"
                | "form"
                | "frame"
                | "frameset"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "head"
                | "header"
                | "hgroup"
                | "hr"
                | "html"
                | "iframe"
                | "img"
                | "input"
                | "keygen"
                | "li"
                | "link"
                | "listing"
                | "main"
                | "marquee"
                | "menu"
                | "meta"
                | "nav"
                | "noembed"
                | "noframes"
                | "noscript"
                | "object"
                | "ol"
                | "p"
                | "param"
                | "plaintext"
                | "pre"
                | "script"
                | "search"
                | "section"
                | "select"
                | "source"
                | "style"
                | "summary"
                | "table"
                | "tbody"
                | "td"
                | "template"
                | "textarea"
                | "tfoot"
                | "th"
                | "thead"
                | "title"
                | "tr"
                | "track"
                | "ul"
                | "wbr"
        ),
    }
}
fn breakout(t: &Tag) -> bool {
    matches!(
        t.name.as_str(),
        "b" | "big"
            | "blockquote"
            | "body"
            | "br"
            | "center"
            | "code"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "em"
            | "embed"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "hr"
            | "i"
            | "img"
            | "li"
            | "listing"
            | "menu"
            | "meta"
            | "nobr"
            | "ol"
            | "p"
            | "pre"
            | "ruby"
            | "s"
            | "small"
            | "span"
            | "strong"
            | "strike"
            | "sub"
            | "sup"
            | "table"
            | "tt"
            | "u"
            | "ul"
            | "var"
    ) || (t.name == "font"
        && t.attributes
            .iter()
            .any(|a| matches!(a.name.as_str(), "color" | "face" | "size")))
}
pub(super) fn element_name(name: &str, ns: Namespace) -> &str {
    if ns == Namespace::Svg {
        SVG_ELEMENTS
            .iter()
            .find(|(from, _)| *from == name)
            .map(|(_, to)| *to)
            .unwrap_or(name)
    } else {
        name
    }
}
fn attribute_name(name: &str, ns: Namespace) -> &str {
    match ns {
        Namespace::MathMl if name == "definitionurl" => "definitionURL",
        Namespace::Svg => SVG_ATTRIBUTES
            .iter()
            .find(|(from, _)| *from == name)
            .map(|(_, to)| *to)
            .unwrap_or(name),
        _ => name,
    }
}
// WHATWG foreign-content case adjustments.
const SVG_ELEMENTS: &[(&str, &str)] = &[
    ("altglyph", "altGlyph"),
    ("altglyphdef", "altGlyphDef"),
    ("altglyphitem", "altGlyphItem"),
    ("animatecolor", "animateColor"),
    ("animatemotion", "animateMotion"),
    ("animatetransform", "animateTransform"),
    ("clippath", "clipPath"),
    ("feblend", "feBlend"),
    ("fecolormatrix", "feColorMatrix"),
    ("fecomponenttransfer", "feComponentTransfer"),
    ("fecomposite", "feComposite"),
    ("feconvolvematrix", "feConvolveMatrix"),
    ("fediffuselighting", "feDiffuseLighting"),
    ("fedisplacementmap", "feDisplacementMap"),
    ("fedistantlight", "feDistantLight"),
    ("fedropshadow", "feDropShadow"),
    ("feflood", "feFlood"),
    ("fefunca", "feFuncA"),
    ("fefuncb", "feFuncB"),
    ("fefuncg", "feFuncG"),
    ("fefuncr", "feFuncR"),
    ("fegaussianblur", "feGaussianBlur"),
    ("feimage", "feImage"),
    ("femerge", "feMerge"),
    ("femergenode", "feMergeNode"),
    ("femorphology", "feMorphology"),
    ("feoffset", "feOffset"),
    ("fepointlight", "fePointLight"),
    ("fespecularlighting", "feSpecularLighting"),
    ("fespotlight", "feSpotLight"),
    ("fetile", "feTile"),
    ("feturbulence", "feTurbulence"),
    ("foreignobject", "foreignObject"),
    ("glyphref", "glyphRef"),
    ("lineargradient", "linearGradient"),
    ("radialgradient", "radialGradient"),
    ("textpath", "textPath"),
];
// WHATWG foreign-content case adjustments.
const SVG_ATTRIBUTES: &[(&str, &str)] = &[
    ("attributename", "attributeName"),
    ("attributetype", "attributeType"),
    ("basefrequency", "baseFrequency"),
    ("baseprofile", "baseProfile"),
    ("calcmode", "calcMode"),
    ("clippathunits", "clipPathUnits"),
    ("diffuseconstant", "diffuseConstant"),
    ("edgemode", "edgeMode"),
    ("filterunits", "filterUnits"),
    ("glyphref", "glyphRef"),
    ("gradienttransform", "gradientTransform"),
    ("gradientunits", "gradientUnits"),
    ("kernelmatrix", "kernelMatrix"),
    ("kernelunitlength", "kernelUnitLength"),
    ("keypoints", "keyPoints"),
    ("keysplines", "keySplines"),
    ("keytimes", "keyTimes"),
    ("lengthadjust", "lengthAdjust"),
    ("limitingconeangle", "limitingConeAngle"),
    ("markerheight", "markerHeight"),
    ("markerunits", "markerUnits"),
    ("markerwidth", "markerWidth"),
    ("maskcontentunits", "maskContentUnits"),
    ("maskunits", "maskUnits"),
    ("numoctaves", "numOctaves"),
    ("pathlength", "pathLength"),
    ("patterncontentunits", "patternContentUnits"),
    ("patterntransform", "patternTransform"),
    ("patternunits", "patternUnits"),
    ("pointsatx", "pointsAtX"),
    ("pointsaty", "pointsAtY"),
    ("pointsatz", "pointsAtZ"),
    ("preservealpha", "preserveAlpha"),
    ("preserveaspectratio", "preserveAspectRatio"),
    ("primitiveunits", "primitiveUnits"),
    ("refx", "refX"),
    ("refy", "refY"),
    ("repeatcount", "repeatCount"),
    ("repeatdur", "repeatDur"),
    ("requiredextensions", "requiredExtensions"),
    ("requiredfeatures", "requiredFeatures"),
    ("specularconstant", "specularConstant"),
    ("specularexponent", "specularExponent"),
    ("spreadmethod", "spreadMethod"),
    ("startoffset", "startOffset"),
    ("stddeviation", "stdDeviation"),
    ("stitchtiles", "stitchTiles"),
    ("surfacescale", "surfaceScale"),
    ("systemlanguage", "systemLanguage"),
    ("tablevalues", "tableValues"),
    ("targetx", "targetX"),
    ("targety", "targetY"),
    ("textlength", "textLength"),
    ("viewbox", "viewBox"),
    ("viewtarget", "viewTarget"),
    ("xchannelselector", "xChannelSelector"),
    ("ychannelselector", "yChannelSelector"),
    ("zoomandpan", "zoomAndPan"),
];
