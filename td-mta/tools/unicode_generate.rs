#![forbid(unsafe_code)]
//! Deterministic cold generation from the approved corpus; no build script.
#[path = "unicode_inputs.rs"]
mod inputs;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    io,
    path::Path,
};

#[derive(Debug, Default)]
struct Tables {
    decomposition: BTreeMap<u32, Vec<u32>>,
    classes: BTreeMap<u32, u8>,
    lowercase: BTreeMap<u32, u32>,
    composition: BTreeMap<(u32, u32), u32>,
}

fn invalid(reason: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}
fn codepoint(text: &str) -> io::Result<u32> {
    if !(4..=6).contains(&text.len())
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
    {
        return Err(invalid("expected uppercase hexadecimal code point"));
    }
    let value = u32::from_str_radix(text, 16).map_err(io::Error::other)?;
    if value > 0x10ffff {
        return Err(invalid("code point out of range"));
    }
    Ok(value)
}
fn scalar(text: &str) -> io::Result<u32> {
    let value = codepoint(text)?;
    char::from_u32(value).ok_or_else(|| invalid("surrogate scalar"))?;
    Ok(value)
}
fn field<'a>(fields: &[&'a str], index: usize) -> io::Result<&'a str> {
    fields
        .get(index)
        .copied()
        .ok_or_else(|| invalid("missing UnicodeData field"))
}
fn range_name(name: &str, ending: &str) -> Option<String> {
    name.strip_prefix('<')?
        .strip_suffix(ending)
        .map(str::to_owned)
}
fn properties<'a, 'b>(fields: &'a [&'b str]) -> io::Result<&'a [&'b str]> {
    fields
        .get(2..)
        .ok_or_else(|| invalid("missing range properties"))
}

impl Tables {
    fn parse(data: &str, props: &str) -> io::Result<Self> {
        let mut tables = Self::default();
        let mut previous = None;
        let mut first: Option<(u32, String, Vec<&str>)> = None;
        for line in data.lines() {
            let fields: Vec<_> = line.split(';').collect();
            if fields.len() != 15 {
                return Err(invalid("UnicodeData must contain fifteen fields"));
            }
            let code = codepoint(field(&fields, 0)?)?;
            if previous.is_some_and(|p| code <= p) {
                return Err(invalid("UnicodeData code points must increase"));
            }
            previous = Some(code);
            let name = field(&fields, 1)?;
            let category = field(&fields, 2)?;
            let class = field(&fields, 3)?.parse::<u8>().map_err(io::Error::other)?;
            let mapping = field(&fields, 5)?;
            let lower = field(&fields, 13)?;
            if let Some((start, base, saved)) = first.take() {
                if range_name(name, ", Last>").as_deref() != Some(&base)
                    || properties(&fields)? != saved
                    || start >= code
                {
                    return Err(invalid("mismatched UnicodeData First/Last range"));
                }
                if category == "Cs" {
                    if !matches!(
                        (start, code),
                        (0xd800, 0xdb7f) | (0xdb80, 0xdbff) | (0xdc00, 0xdfff)
                    ) {
                        return Err(invalid("unexpected surrogate range"));
                    }
                } else if (start..=code).contains(&0xd800) || (0xd800..=0xdfff).contains(&start) {
                    return Err(invalid("scalar range crosses surrogates"));
                }
                continue;
            }
            if let Some(base) = range_name(name, ", First>") {
                if class != 0 || !mapping.is_empty() || !lower.is_empty() {
                    return Err(invalid("range has unsupported nondefault table properties"));
                }
                first = Some((code, base, properties(&fields)?.to_vec()));
                continue;
            }
            if range_name(name, ", Last>").is_some() {
                return Err(invalid("unpaired UnicodeData Last record"));
            }
            char::from_u32(code).ok_or_else(|| invalid("surrogate outside First/Last range"))?;
            if category == "Cs" {
                return Err(invalid("scalar has surrogate category"));
            }
            if class != 0 {
                tables.classes.insert(code, class);
            }
            if !lower.is_empty() {
                tables.lowercase.insert(code, scalar(lower)?);
            }
            if !mapping.is_empty() {
                let mut parts = mapping.split_ascii_whitespace();
                let head = parts.next().ok_or_else(|| invalid("empty decomposition"))?;
                let compatibility = head.starts_with('<');
                if compatibility && (!head.ends_with('>') || head.len() < 3) {
                    return Err(invalid("malformed compatibility tag"));
                }
                let mut values = Vec::new();
                if !compatibility {
                    values.push(scalar(head)?);
                }
                for value in parts {
                    values.push(scalar(value)?);
                }
                if values.is_empty() || (!compatibility && values.len() > 2) {
                    return Err(invalid("invalid decomposition arity"));
                }
                if !compatibility {
                    tables.decomposition.insert(code, values);
                }
            }
        }
        if first.is_some() {
            return Err(invalid("unterminated UnicodeData range"));
        }
        let excluded = exclusions(props)?;
        for (&code, mapping) in &tables.decomposition {
            if !excluded.contains(&code) {
                let (Some(&left), Some(&right)) = (mapping.first(), mapping.get(1)) else {
                    return Err(invalid(
                        "nonexcluded canonical mapping must have two scalars",
                    ));
                };
                if tables.classes.contains_key(&left) {
                    return Err(invalid("nonexcluded composition begins with nonstarter"));
                }
                if tables.composition.insert((left, right), code).is_some() {
                    return Err(invalid("duplicate canonical composition pair"));
                }
            }
        }
        tables.decomposition = expand(&tables.decomposition)?;
        Ok(tables)
    }

    fn render(&self, license: &str) -> io::Result<String> {
        let mut out = String::new();
        for line in license.lines() {
            if line.is_empty() {
                writeln!(out, "//").map_err(io::Error::other)?;
            } else {
                writeln!(out, "// {line}").map_err(io::Error::other)?;
            }
        }
        writeln!(
            out,
            "// Generated by examples/unicode_generate.rs from Unicode 17.0.0."
        )
        .map_err(io::Error::other)?;
        writeln!(
            out,
            "// Do not edit; UNICODE.md pins inputs and regeneration."
        )
        .map_err(io::Error::other)?;
        writeln!(out, "\npub const DECOMPOSITION: &[(u32, u16, u8)] = &[")
            .map_err(io::Error::other)?;
        let mut values = Vec::new();
        for (&code, mapping) in &self.decomposition {
            let offset = u16::try_from(values.len()).map_err(io::Error::other)?;
            let len = u8::try_from(mapping.len()).map_err(io::Error::other)?;
            writeln!(out, "    (0x{code:04X}, {offset}, {len}),").map_err(io::Error::other)?;
            values.extend_from_slice(mapping);
        }
        u16::try_from(values.len()).map_err(io::Error::other)?;
        writeln!(out, "];\npub const DECOMPOSED: &[u32] = &[").map_err(io::Error::other)?;
        // Fixed numeric tokens packed to the repository's default 100 columns.
        let mut line = String::from("    ");
        for code in values {
            let token = format!("0x{code:06X},");
            if line.len() > 4 && line.len() + 1 + token.len() > 100 {
                writeln!(out, "{line}").map_err(io::Error::other)?;
                line.clear();
                line.push_str("    ");
            }
            if line.len() > 4 {
                line.push(' ');
            }
            line.push_str(&token);
        }
        if line.len() > 4 {
            writeln!(out, "{line}").map_err(io::Error::other)?;
        }
        writeln!(out, "];\npub const CLASSES: &[(u32, u32, u8)] = &[").map_err(io::Error::other)?;
        let mut range: Option<(u32, u32, u8)> = None;
        for (&code, &class) in &self.classes {
            if let Some((_, end, old_class)) = range.as_mut() {
                if end.checked_add(1) == Some(code) && *old_class == class {
                    *end = code;
                    continue;
                }
            }
            if let Some((start, end, class)) = range {
                writeln!(out, "    (0x{start:04X}, 0x{end:04X}, {class}),")
                    .map_err(io::Error::other)?;
            }
            range = Some((code, code, class));
        }
        if let Some((start, end, class)) = range {
            writeln!(out, "    (0x{start:04X}, 0x{end:04X}, {class}),")
                .map_err(io::Error::other)?;
        }
        writeln!(out, "];\npub const COMPOSITION: &[(u32, u32, u32)] = &[")
            .map_err(io::Error::other)?;
        for (&(left, right), &code) in &self.composition {
            writeln!(out, "    (0x{left:04X}, 0x{right:04X}, 0x{code:04X}),")
                .map_err(io::Error::other)?;
        }
        writeln!(out, "];\npub const LOWERCASE: &[(u32, u32)] = &[").map_err(io::Error::other)?;
        for (&code, &lower) in &self.lowercase {
            writeln!(out, "    (0x{code:04X}, 0x{lower:04X}),").map_err(io::Error::other)?;
        }
        writeln!(out, "];").map_err(io::Error::other)?;
        Ok(out)
    }
}

fn exclusions(text: &str) -> io::Result<BTreeSet<u32>> {
    let mut result = BTreeSet::new();
    let mut previous = None;
    for line in text.lines() {
        let content = line.split('#').next().unwrap_or_default().trim();
        let fields: Vec<_> = content.split(';').map(str::trim).collect();
        if fields.get(1) != Some(&"Full_Composition_Exclusion") {
            continue;
        }
        if fields.len() != 2 {
            return Err(invalid("invalid Full_Composition_Exclusion record"));
        }
        let span = field(&fields, 0)?;
        let (start, end) = if let Some((start, end)) = span.split_once("..") {
            (scalar(start)?, scalar(end)?)
        } else {
            let value = scalar(span)?;
            (value, value)
        };
        if start > end || previous.is_some_and(|p| start <= p) || (start..=end).contains(&0xd800) {
            return Err(invalid("unsorted, overlapping or invalid exclusion range"));
        }
        previous = Some(end);
        result.extend(start..=end);
    }
    Ok(result)
}

fn expand(raw: &BTreeMap<u32, Vec<u32>>) -> io::Result<BTreeMap<u32, Vec<u32>>> {
    let mut done: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut active = BTreeSet::new();
    for &root in raw.keys() {
        let mut stack = vec![(root, false)];
        while let Some((code, finish)) = stack.pop() {
            if done.contains_key(&code) {
                continue;
            }
            let mapping = raw
                .get(&code)
                .ok_or_else(|| invalid("missing decomposition node"))?;
            if finish {
                let mut flattened = Vec::new();
                for child in mapping {
                    if let Some(expanded) = done.get(child) {
                        flattened.extend_from_slice(expanded);
                    } else if raw.contains_key(child) {
                        return Err(invalid("unfinished decomposition dependency"));
                    } else if (0xac00..=0xd7a3).contains(child) {
                        let index = child - 0xac00;
                        flattened.push(0x1100 + index / 588);
                        flattened.push(0x1161 + (index % 588) / 28);
                        if index % 28 != 0 {
                            flattened.push(0x11a7 + index % 28);
                        }
                    } else {
                        flattened.push(*child);
                    }
                    if flattened.len() > 4 {
                        return Err(invalid("canonical decomposition exceeds four scalars"));
                    }
                }
                active.remove(&code);
                done.insert(code, flattened);
            } else {
                if !active.insert(code) {
                    return Err(invalid("cyclic canonical decomposition"));
                }
                stack.push((code, true));
                for &child in mapping.iter().rev() {
                    if raw.contains_key(&child) && !done.contains_key(&child) {
                        stack.push((child, false));
                    }
                }
            }
        }
    }
    Ok(done)
}

pub fn generate(directory: &Path) -> io::Result<String> {
    let inputs = inputs::load(directory)?;
    let source = |name| {
        inputs
            .iter()
            .find(|input| input.pin.name == name)
            .map(|input| input.text.as_str())
            .ok_or_else(|| invalid("missing approved input"))
    };
    let tables = Tables::parse(
        source("UnicodeData.txt")?,
        source("DerivedNormalizationProps.txt")?,
    )?;
    if tables
        .classes
        .values()
        .copied()
        .collect::<BTreeSet<_>>()
        .len()
        != 55
    {
        return Err(invalid(
            "Unicode 17 must contain 55 nonzero combining classes",
        ));
    }
    tables.render(source("license.txt")?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
    use super::*;
    fn row(
        code: &str,
        name: &str,
        category: &str,
        class: &str,
        mapping: &str,
        lower: &str,
    ) -> String {
        [
            code, name, category, class, "L", mapping, "", "", "", "N", "", "", "", lower, "",
        ]
        .join(";")
            + "\n"
    }
    #[test]
    fn scalar_records_select_canonical_pairs_and_simple_lowercase() {
        let data = row("0041", "CAPITAL A", "Lu", "0", "", "0061")
            + &row("00A0", "SPACE", "Zs", "0", "<noBreak> 0020", "")
            + &row("00C0", "A GRAVE", "Lu", "0", "0041 0300", "00E0")
            + &row("00C5", "A RING", "Lu", "0", "0041 030A", "00E5")
            + &row("01FA", "A RING ACUTE", "Lu", "0", "00C5 0301", "01FB")
            + &row("0300", "GRAVE", "Mn", "230", "", "")
            + &row("0340", "GRAVE TONE", "Mn", "230", "0300", "")
            + &row("212B", "ANGSTROM", "Lu", "0", "00C5", "00E5");
        let table = Tables::parse(
            &data,
            "0340 ; Full_Composition_Exclusion\n212B ; Full_Composition_Exclusion\n",
        )
        .unwrap();
        assert_eq!(table.lowercase.get(&0x41), Some(&0x61));
        assert!(!table.decomposition.contains_key(&0xa0));
        assert_eq!(
            table.decomposition.get(&0x1fa).unwrap(),
            &[0x41, 0x30a, 0x301]
        );
        assert_eq!(table.decomposition.get(&0x212b).unwrap(), &[0x41, 0x30a]);
        assert_eq!(table.composition.len(), 3);
        assert_eq!(table.composition.get(&(0xc5, 0x301)), Some(&0x1fa));
        assert!(!table
            .composition
            .values()
            .any(|c| *c == 0x340 || *c == 0x212b));
        assert_eq!(table.classes.get(&0x300), Some(&230));
        assert!(Tables::parse(&data, "").is_err());
    }
    #[test]
    fn malformed_records_ranges_and_exclusions_refuse() {
        let single = row("0041", "A", "Lu", "0", "", "");
        let valid_range = row("3400", "<CJK, First>", "Lo", "0", "", "")
            + &row("4DBF", "<CJK, Last>", "Lo", "0", "", "");
        let surrogate = row("D800", "<High, First>", "Cs", "0", "", "")
            + &row("DB7F", "<High, Last>", "Cs", "0", "", "");
        assert!(Tables::parse(&(valid_range.clone() + &surrogate), "").is_ok());
        for bad in [
            "0041;A".to_owned(),
            single.clone() + &single,
            row("004G", "A", "Lu", "0", "", ""),
            row("110000", "A", "Lu", "0", "", ""),
            row("D800", "A", "Cs", "0", "", ""),
            row("0041", "A", "Cs", "0", "", ""),
            row("0041", "A", "Lu", "256", "", ""),
            row("0041", "A", "Lu", "0", "", "D800"),
            row("0041", "A", "Lu", "0", "D800", ""),
            row("0041", "A", "Lu", "0", "0042 0043 0044", ""),
            row("0041", "A", "Lu", "0", "<bad 0042", ""),
            row("0041", "A", "Lu", "0", "<wide>", ""),
            row("3400", "<CJK, First>", "Lo", "0", "", ""),
            row("4DBF", "<CJK, Last>", "Lo", "0", "", ""),
            valid_range.replace("CJK, Last", "Other, Last"),
            valid_range.replace(";Lo;0;", ";Lo;1;"),
            surrogate.replace("DB7F", "DBFF"),
            surrogate.replace(";Cs;", ";Lo;"),
        ] {
            assert!(Tables::parse(&bad, "").is_err(), "accepted {bad:?}");
        }
        for last in [
            row("4DBF", "<CJK, Last>", "Lo", "1", "", ""),
            row("4DBF", "<CJK, Last>", "Lo", "0", "0041", ""),
            row("4DBF", "<CJK, Last>", "Lo", "0", "", "0061"),
        ] {
            let mismatched = row("3400", "<CJK, First>", "Lo", "0", "", "") + &last;
            assert!(Tables::parse(&mismatched, "").is_err());
        }
        for bad in [
            "0341..0340 ; Full_Composition_Exclusion",
            "0340 ; Full_Composition_Exclusion\n0340 ; Full_Composition_Exclusion",
            "0340 ; Full_Composition_Exclusion ; extra",
            "D800 ; Full_Composition_Exclusion",
            "D7FF..E000 ; Full_Composition_Exclusion",
        ] {
            assert!(exclusions(bad).is_err(), "accepted {bad}");
        }
        assert_eq!(
            exclusions("# comment\n0340..0341 ; Full_Composition_Exclusion # Mn\n0342 ; Other\n")
                .unwrap(),
            BTreeSet::from([0x340, 0x341])
        );
        let duplicate = row("00C0", "A", "Lu", "0", "0041 0300", "")
            + &row("00C1", "B", "Lu", "0", "0041 0300", "");
        assert!(Tables::parse(&duplicate, "").is_err());
    }
    #[test]
    fn expansion_is_iterative_bounded_and_rejects_cycles() {
        let nested = BTreeMap::from([(0x1000, vec![0x1001, 0x1001]), (0x1001, vec![0x41, 0x301])]);
        assert_eq!(
            expand(&nested).unwrap().get(&0x1000).unwrap(),
            &[0x41, 0x301, 0x41, 0x301]
        );
        assert!(expand(&BTreeMap::from([(0x1000, vec![0x1000])])).is_err());
        assert!(expand(&BTreeMap::from([
            (0x1000, vec![0x1001]),
            (0x1001, vec![0x1000])
        ]))
        .is_err());
        let mut over = nested;
        over.insert(0x1002, vec![0x1000, 0x41]);
        assert!(expand(&over).is_err());
        let mut chain = BTreeMap::new();
        for code in 0x1000..0x1800 {
            chain.insert(code, vec![code + 1]);
        }
        assert_eq!(expand(&chain).unwrap().get(&0x1000).unwrap(), &[0x1800]);
        assert_eq!(
            expand(&BTreeMap::from([(0x1000, vec![0xac01])]))
                .unwrap()
                .get(&0x1000)
                .unwrap(),
            &[0x1100, 0x1161, 0x11a8]
        );
    }
}
