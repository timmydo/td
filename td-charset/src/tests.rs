#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

fn check(encoding: Encoding, bytes: &[u8], expected: &str, errors: bool) {
    assert_eq!(
        decode(encoding, bytes, expected.len()).unwrap(),
        Text {
            text: expected.into(),
            had_errors: errors
        }
    );
    for split in 0..=bytes.len() {
        let mut decoder = Decoder::new(encoding);
        let mut text = String::new();
        let mut had_errors = false;
        for part in [&bytes[..split], &bytes[split..]] {
            for byte in part {
                let batch = decoder.push(*byte).unwrap();
                text.extend(batch.chars());
                had_errors |= batch.had_errors;
            }
        }
        let batch = decoder.finish();
        text.extend(batch.chars());
        had_errors |= batch.had_errors;
        assert_eq!((text.as_str(), had_errors), (expected, errors));
        assert_eq!(decoder.finish().chars().count(), 0);
        assert_eq!(decoder.push(0), Err(Finished));
    }
}
#[test]
fn labels_and_aliases() {
    for (label, encoding) in labels::LABELS {
        assert_eq!(Encoding::for_label(label), Some(*encoding));
        assert_eq!(
            Encoding::for_label(encoding.name().as_bytes()),
            Some(*encoding)
        );
    }
    assert_eq!(
        Encoding::for_label(b" \tISO-8859-1\r\n"),
        Some(Encoding::Windows1252)
    );
    assert_eq!(Encoding::for_label(b"ascii"), Some(Encoding::Windows1252));
    assert_eq!(
        Encoding::for_label(b"iso-8859-9"),
        Some(Encoding::Windows1254)
    );
    assert_eq!(
        Encoding::for_label(b"iso-8859-11"),
        Some(Encoding::Windows874)
    );
    for label in [
        b"\x0butf8".as_slice(),
        b"utf-7",
        b"iso-8859-12",
        b"big5",
        b"euc-jp",
        b"iso-2022-jp",
        b"utf8\0",
        b"",
    ] {
        assert_eq!(Encoding::for_label(label), None);
    }
}
#[test]
fn languages_and_special_mappings() {
    check(
        Encoding::Windows1252,
        b"caf\xe9 \x80\x81\x93",
        "café €\u{81}“",
        false,
    );
    check(Encoding::Iso88592, b"\xa1\xb1\xa3\xb3", "ĄąŁł", false);
    check(
        Encoding::Iso88595,
        b"\xbf\xe0\xd8\xd2\xd5\xe2",
        "Привет",
        false,
    );
    check(Encoding::Iso88598, b"\xf9\xec\xe5\xed", "שלום", false);
    check(Encoding::Iso88597, b"\xc1\xe8\xde\xed\xe1", "Αθήνα", false);
    check(
        Encoding::ShiftJis,
        b"\x93\xfa\x96\x7b\x8c\xea",
        "日本語",
        false,
    );
    check(
        Encoding::ShiftJis,
        b"\x80\xa1\xdf\xf0\x40\xf9\xfc",
        "\u{80}｡ﾟ\u{e000}\u{e757}",
        false,
    );
    check(
        Encoding::EucKr,
        b"\xc7\xd1\xb1\xdb\x81\x41",
        "한글갂",
        false,
    );
    check(
        Encoding::Gbk,
        b"\xd6\xd0\xce\xc4\x80\xa3\xa0",
        "中文€\u{3000}",
        false,
    );
    check(Encoding::Gbk, b"\x90\x30\x81\x30", "\u{10000}", false);
    check(Encoding::Gb18030, b"\x81\x30\x81\x30", "\u{80}", false);
    check(
        Encoding::UserDefined,
        b"a\x80\xff",
        "a\u{f780}\u{f7ff}",
        false,
    );
}
#[test]
fn malformed_and_eof() {
    for encoding in [
        Encoding::ShiftJis,
        Encoding::EucKr,
        Encoding::Gbk,
        Encoding::Gb18030,
    ] {
        check(encoding, b"\x81\x22", "�\"", true);
        check(encoding, b"\x81", "�", true);
    }
    check(Encoding::Gbk, b"\x81\x30\x81\x22", "�0�\"", true);
    check(Encoding::Gbk, b"\x81\x30\xff", "�0�", true);
    check(Encoding::Gbk, b"\x81\x30", "�", true);
    check(Encoding::Gbk, b"\x81\x30\x81", "�", true);
    check(Encoding::Gbk, b"\xfe\x39\xfe\x39", "�", true);
    check(Encoding::EucKr, b"\x81\x5b", "�[", true);
    check(Encoding::Iso88593, b"\xa5", "�", true);
    check(
        Encoding::Utf8,
        b"\xe0\x80\x22\xed\xa0\x80\xf4\x90\x80\x80",
        "��\"�������",
        true,
    );
    check(Encoding::Utf8, b"\xf0\x90\x80", "�", true);
    check(Encoding::Utf8, "é💩�".as_bytes(), "é💩�", false);
    check(Encoding::Replacement, b"any text", "�", true);
    check(Encoding::Replacement, b"", "", false);
}
#[test]
fn utf16_and_bom_preservation() {
    check(
        Encoding::Utf16Le,
        b"\xff\xfeA\0\x3d\xd8\xa9\xdc",
        "\u{feff}A💩",
        false,
    );
    check(
        Encoding::Utf16Be,
        b"\xfe\xff\0A\xd8\x3d\xdc\xa9",
        "\u{feff}A💩",
        false,
    );
    check(Encoding::Utf16Le, b"\0\xd8A\0\0\xdcB", "�A��", true);
    check(Encoding::Utf16Le, b"\0\xd8A", "�", true);
    check(Encoding::Utf8, b"\xef\xbb\xbf", "\u{feff}", false);
}
#[test]
fn output_ceiling() {
    assert_eq!(decode(Encoding::Windows1252, b"\x80", 2), Err(OutputLimit));
    assert_eq!(
        decode(Encoding::Gbk, b"\x81\x30\x81\x22", 7),
        Err(OutputLimit)
    );
    assert_eq!(decode(Encoding::Utf8, b"", 0).unwrap().text, "");
}
#[test]
fn sniff_precedence_and_policy() {
    use sniff::*;
    let s = sniff(
        b"\xff\xfeA\0",
        Some(b"shift_jis"),
        true,
        Encoding::Windows1252,
    );
    assert_eq!(
        (s.encoding, s.source, s.bom_len),
        (Encoding::Utf16Le, Source::Bom, 2)
    );
    assert_eq!(
        decode(s.encoding, s.body(b"\xff\xfeA\0").unwrap(), 1)
            .unwrap()
            .text,
        "A"
    );
    assert_eq!(
        sniff(
            b"<meta charset=gbk>",
            Some(b"utf8"),
            true,
            Encoding::Windows1252
        )
        .source,
        Source::Declared
    );
    assert_eq!(
        sniff(
            b"<meta charset=gbk>",
            Some(b"bad"),
            true,
            Encoding::Windows1252
        )
        .encoding,
        Encoding::Gbk
    );
    assert_eq!(
        sniff(b"<meta charset=gbk>", None, false, Encoding::Utf8).source,
        Source::Fallback
    );
    assert_eq!(
        sniff("日本語".as_bytes(), None, false, Encoding::Windows1252).encoding,
        Encoding::Windows1252
    );
}
#[test]
fn meta_attributes_comments_and_bounds() {
    use sniff::html_meta as meta;
    for bytes in [
        b"<META CHARSET = 'shift_jis'>".as_slice(),
        b"<meta content='text/html; charset=shift_jis' http-equiv=content-type>",
        b"<meta http-equiv=CONTENT-TYPE content=charset=shift_jis>",
        b"<meta charset=Shift_JIS charset=gbk>",
        b"<!-- <meta charset=gbk> --><meta/charset=shift_jis>",
        b"<div title='<meta charset=gbk>'><meta charset=shift_jis>",
    ] {
        assert_eq!(meta(bytes), Some(Encoding::ShiftJis), "{bytes:?}");
    }
    for bytes in [
        b"<meta content='charset=gbk'>".as_slice(),
        b"<metadata charset=gbk>",
        b"<!-- <meta charset=gbk>",
        b"<meta charset='gbk",
        b"<meta charset=gbk",
        b"<meta charset=bad charset=gbk>",
        b"<meta charset=bad content='charset=gbk' http-equiv=content-type>",
        b"<meta http-equiv=bad http-equiv=content-type content='charset=gbk'>",
    ] {
        assert_eq!(meta(bytes), None, "{bytes:?}");
    }
    assert_eq!(
        meta(b"<meta charset=bad><meta charset=gbk>"),
        Some(Encoding::Gbk)
    );
    assert_eq!(meta(b"<meta charset=utf-16be>"), Some(Encoding::Utf8));
    assert_eq!(
        meta(b"<meta charset=x-user-defined>"),
        Some(Encoding::Windows1252)
    );
    let tag = b"<meta charset=gbk>";
    let mut bytes = vec![b' '; 1024 - tag.len()];
    bytes.extend(tag);
    assert_eq!(meta(&bytes), Some(Encoding::Gbk));
    bytes.insert(0, b' ');
    assert_eq!(meta(&bytes), None);
}

fn entry(table: &[u8], index: usize) -> Option<char> {
    let bytes: [u8; 4] = table.get(index * 4..index * 4 + 4)?.try_into().ok()?;
    char::from_u32(u32::from_le_bytes(bytes))
}
#[test]
fn every_single_byte_and_multibyte_index_entry() {
    for (_, encoding) in labels::LABELS {
        if let Some(table) = encoding.single_byte() {
            for byte in 0..=255u8 {
                let expected = if byte < 128 {
                    Some(char::from(byte))
                } else {
                    entry(table, usize::from(byte - 128))
                };
                let decoded = decode(*encoding, &[byte], 4).unwrap();
                assert_eq!(decoded.text, expected.unwrap_or('�').to_string());
                assert_eq!(decoded.had_errors, expected.is_none());
            }
        }
    }
    for (encoding, table, width) in [
        (
            Encoding::ShiftJis,
            include_bytes!("../data/jis0208.bin").as_slice(),
            188,
        ),
        (
            Encoding::EucKr,
            include_bytes!("../data/euc-kr.bin").as_slice(),
            190,
        ),
        (
            Encoding::Gbk,
            include_bytes!("../data/gb18030.bin").as_slice(),
            190,
        ),
    ] {
        let count = if encoding == Encoding::ShiftJis {
            60 * 188
        } else {
            126 * 190
        };
        for pointer in 0..count {
            let row = pointer / width;
            let col = pointer % width;
            let (lead, trail) = match encoding {
                Encoding::ShiftJis => (
                    row + if row < 31 { 0x81 } else { 0xc1 },
                    col + if col < 63 { 0x40 } else { 0x41 },
                ),
                Encoding::EucKr => (row + 0x81, col + 0x41),
                _ => (row + 0x81, col + if col < 63 { 0x40 } else { 0x41 }),
            };
            let value = if encoding == Encoding::ShiftJis && (8836..=10715).contains(&pointer) {
                char::from_u32(0xe000 + (pointer - 8836) as u32)
            } else {
                entry(table, pointer)
            };
            let mut expected = value.unwrap_or('�').to_string();
            if value.is_none() && trail < 128 {
                expected.push(char::from(trail as u8));
            }
            assert_eq!(
                decode(encoding, &[lead as u8, trail as u8], 8).unwrap(),
                Text {
                    text: expected,
                    had_errors: value.is_none()
                },
                "{encoding:?} pointer {pointer}"
            );
        }
    }
}
#[test]
fn every_gb18030_four_byte_pointer() {
    let table = include_bytes!("../data/gb18030-ranges.bin");
    let word = |index| u32::from_le_bytes(table[index * 4..index * 4 + 4].try_into().unwrap());
    let mut range = 0;
    for pointer in 0..126 * 10 * 126 * 10u32 {
        while range + 1 < table.len() / 8 && word((range + 1) * 2) <= pointer {
            range += 1;
        }
        let value = if (39420..189000).contains(&pointer) || pointer > 1237575 {
            None
        } else if pointer == 7457 {
            Some('\u{e7c7}')
        } else {
            char::from_u32(word(range * 2 + 1) + pointer - word(range * 2))
        };
        let bytes = [
            (pointer / 12600 + 0x81) as u8,
            (pointer / 1260 % 10 + 0x30) as u8,
            (pointer / 10 % 126 + 0x81) as u8,
            (pointer % 10 + 0x30) as u8,
        ];
        let decoded = decode(Encoding::Gbk, &bytes, 4).unwrap();
        assert_eq!(
            decoded.text,
            value.unwrap_or('�').to_string(),
            "pointer {pointer}"
        );
        assert_eq!(decoded.had_errors, value.is_none());
    }
}
#[test]
fn every_two_byte_input_is_bounded_and_finishes() {
    for encoding in [
        Encoding::Utf8,
        Encoding::Utf16Le,
        Encoding::Utf16Be,
        Encoding::ShiftJis,
        Encoding::EucKr,
        Encoding::Gbk,
    ] {
        for pair in 0..=u16::MAX {
            let bytes = pair.to_be_bytes();
            let result = decode(encoding, &bytes, 8).unwrap();
            assert!(result.text.chars().count() <= 2);
        }
    }
}
