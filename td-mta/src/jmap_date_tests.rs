#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

#[test]
fn literal_dates_preserve_milliseconds_and_four_digit_year_boundaries() {
    for (milliseconds, expected) in [
        (-62_167_219_200_000, "0000-01-01T00:00:00Z"),
        (-62_162_121_600_000, "0000-02-29T00:00:00Z"),
        (1_746_515_289_001, "2025-05-06T07:08:09.001Z"),
        (-62_135_596_800_000, "0001-01-01T00:00:00Z"),
        (-2_208_988_800_000, "1900-01-01T00:00:00Z"),
        (-1001, "1969-12-31T23:59:58.999Z"),
        (-1000, "1969-12-31T23:59:59Z"),
        (-999, "1969-12-31T23:59:59.001Z"),
        (-1, "1969-12-31T23:59:59.999Z"),
        (0, "1970-01-01T00:00:00Z"),
        (1, "1970-01-01T00:00:00.001Z"),
        (10, "1970-01-01T00:00:00.010Z"),
        (100, "1970-01-01T00:00:00.100Z"),
        (999, "1970-01-01T00:00:00.999Z"),
        (1000, "1970-01-01T00:00:01Z"),
        (951_782_400_001, "2000-02-29T00:00:00.001Z"),
        (253_402_300_799_999, "9999-12-31T23:59:59.999Z"),
    ] {
        let mut bytes = [0; 24];
        let mut output = TextBuffer::new(&mut bytes);
        write_utc_ms(milliseconds, &mut output).unwrap();
        assert_eq!(output.as_str().unwrap(), expected);
    }
}

#[test]
fn every_short_output_refuses_atomically_and_exact_capacity_succeeds() {
    for (milliseconds, expected) in [
        (0, "1970-01-01T00:00:00Z"),
        (-1, "1969-12-31T23:59:59.999Z"),
    ] {
        for capacity in 0..=expected.len() {
            let mut bytes = [0; 32];
            let mut output = TextBuffer::new(&mut bytes[..capacity + 3]);
            output.append("pre").unwrap();
            if capacity < expected.len() {
                assert_eq!(
                    write_utc_ms(milliseconds, &mut output),
                    Err(Error::Output(crate::bounded::Error::Capacity))
                );
                assert_eq!(output.as_str().unwrap(), "pre");
            } else {
                write_utc_ms(milliseconds, &mut output).unwrap();
                assert_eq!(output.as_str().unwrap(), format!("pre{expected}"));
            }
        }
    }
    for milliseconds in [i64::MIN, -62_167_219_200_001, 253_402_300_800_000, i64::MAX] {
        let mut bytes = [0; 32];
        let mut output = TextBuffer::new(&mut bytes);
        output.append("pre").unwrap();
        assert_eq!(
            write_utc_ms(milliseconds, &mut output),
            Err(Error::OutOfRange)
        );
        assert_eq!(output.as_str().unwrap(), "pre");
    }
}

#[test]
fn every_millisecond_around_epoch_keeps_euclidean_second_and_fraction() {
    for milliseconds in -2000i64..=2000 {
        let mut bytes = [0; 24];
        let mut output = TextBuffer::new(&mut bytes);
        write_utc_ms(milliseconds, &mut output).unwrap();
        let text = output.as_str().unwrap();
        assert_eq!(
            td_civil::parse_rfc3339(text),
            Some(milliseconds.div_euclid(1000))
        );
        let fraction = milliseconds.rem_euclid(1000);
        if fraction == 0 {
            assert_eq!(text.len(), 20);
            assert!(!text.contains('.'));
        } else {
            assert_eq!(text.len(), 24);
            assert!(text.ends_with(&format!(".{fraction:03}Z")));
        }
    }
}
