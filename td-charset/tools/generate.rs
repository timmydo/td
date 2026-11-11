//! Cold table generation from SHA-256-pinned WHATWG data, never a build step.
#![forbid(unsafe_code)]
#[path = "../../engine/src/json.rs"]
#[allow(dead_code)]
mod json;
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;
use json::Json;
use std::{fmt::Write as _, io, path::Path};

fn invalid() -> io::Error {
    io::Error::other("invalid pinned encoding data")
}
fn read(directory: &Path, file: &str, length: usize, hash: &str) -> io::Result<Json> {
    use std::io::Read;
    let file = std::fs::File::open(directory.join(file))?;
    let mut bytes = Vec::new();
    file.take((length + 1) as u64).read_to_end(&mut bytes)?;
    let mut digest = sha256::Sha256::new();
    digest.update(&bytes);
    let mut hex = String::new();
    for b in digest.finalize() {
        write!(hex, "{b:02x}").map_err(io::Error::other)?;
    }
    if bytes.len() != length || hex != hash {
        return Err(invalid());
    }
    json::parse(std::str::from_utf8(&bytes).map_err(io::Error::other)?).map_err(io::Error::other)
}
fn array(value: &Json) -> io::Result<&[Json]> {
    value.as_arr().ok_or_else(invalid)
}
fn string(value: &Json) -> io::Result<&str> {
    value.as_str().ok_or_else(invalid)
}
fn member<'a>(value: &'a Json, key: &str) -> io::Result<&'a Json> {
    value.get(key).ok_or_else(invalid)
}
fn variant(name: &str) -> String {
    match name {
        "UTF-8" => "Utf8".into(),
        "UTF-16LE" => "Utf16Le".into(),
        "UTF-16BE" => "Utf16Be".into(),
        "Shift_JIS" => "ShiftJis".into(),
        "EUC-KR" => "EucKr".into(),
        "GBK" => "Gbk".into(),
        "gb18030" => "Gb18030".into(),
        "x-user-defined" => "UserDefined".into(),
        "replacement" => "Replacement".into(),
        _ => name
            .split('-')
            .map(|s| {
                let mut chars = s.chars();
                chars
                    .next()
                    .map_or_else(String::new, |c| c.to_uppercase().to_string())
                    + &chars.as_str().to_ascii_lowercase()
            })
            .collect(),
    }
}
fn index_name(name: &str) -> String {
    if name == "ISO-8859-8-I" {
        "iso-8859-8".into()
    } else {
        name.to_ascii_lowercase()
    }
}
fn number(value: &Json) -> io::Result<u32> {
    match value {
        Json::Null => Ok(u32::MAX),
        Json::Num(n) => n.parse().map_err(io::Error::other),
        _ => Err(invalid()),
    }
}
/// Returns relative artifact paths and deterministic bytes. Rust output needs
/// rustfmt, like all repository Rust sources; binary tables need no transform.
pub fn generate(directory: &Path) -> io::Result<Vec<(String, Vec<u8>)>> {
    let indexes = read(
        directory,
        "indexes.json",
        530213,
        "b7c8961095f0b4cae8f4a16a5011e7c84047a60c1277e28ee8df9e56074ee509",
    )?;
    let encodings = read(
        directory,
        "encodings.json",
        8913,
        "078212b3697f60b81225b6671bd9da2604497abff0aa8e96e4d46605c02ac9e7",
    )?;
    let mut selected = Vec::new();
    for group in array(&encodings)? {
        for encoding in array(member(group, "encodings")?)? {
            let name = string(member(encoding, "name")?)?;
            let single = indexes
                .get(&index_name(name))
                .and_then(Json::as_arr)
                .is_some_and(|a| a.len() == 128);
            if single
                || matches!(
                    name,
                    "UTF-8"
                        | "UTF-16LE"
                        | "UTF-16BE"
                        | "Shift_JIS"
                        | "EUC-KR"
                        | "GBK"
                        | "gb18030"
                        | "x-user-defined"
                        | "replacement"
                )
            {
                selected.push((name, array(member(encoding, "labels")?)?, single));
            }
        }
    }
    let mut code = String::from("// WHATWG label data; generated from the pinned encodings.json (data/README.md).\n#[derive(Clone, Copy, Debug, Eq, PartialEq)]\npub enum Encoding {\n");
    for (name, _, _) in &selected {
        writeln!(code, "    {},", variant(name)).map_err(io::Error::other)?;
    }
    code.push_str(
        "}\nimpl Encoding {\n    pub fn name(self) -> &'static str {\n        match self {\n",
    );
    for (name, _, _) in &selected {
        writeln!(code, "            Self::{} => {name:?},", variant(name))
            .map_err(io::Error::other)?;
    }
    code.push_str("        }\n    }\n    pub(crate) fn single_byte(self) -> Option<&'static [u8]> {\n        match self {\n");
    for (name, _, single) in &selected {
        if *single {
            writeln!(
                code,
                "            Self::{} => Some(include_bytes!(\"../data/{}.bin\")),",
                variant(name),
                index_name(name)
            )
            .map_err(io::Error::other)?;
        }
    }
    code.push_str("            _ => None,\n        }\n    }\n}\npub(crate) const LABELS: &[(&[u8], Encoding)] = &[\n");
    for (name, labels, _) in &selected {
        for label in *labels {
            writeln!(
                code,
                "    (b{:?}, Encoding::{}),",
                string(label)?,
                variant(name)
            )
            .map_err(io::Error::other)?;
        }
    }
    code.push_str("];\n");
    let mut result = vec![("src/labels.rs".into(), code.into_bytes())];
    let Json::Obj(indexes) = &indexes else {
        return Err(invalid());
    };
    for (name, value) in indexes {
        let values = array(value)?;
        if values.len() != 128
            && !matches!(
                name.as_str(),
                "jis0208" | "euc-kr" | "gb18030" | "gb18030-ranges"
            )
        {
            continue;
        }
        let mut bytes = Vec::new();
        for value in values {
            if name == "gb18030-ranges" {
                for n in array(value)? {
                    bytes.extend(number(n)?.to_le_bytes());
                }
            } else {
                bytes.extend(number(value)?.to_le_bytes());
            }
        }
        result.push((format!("data/{name}.bin"), bytes));
    }
    Ok(result)
}
