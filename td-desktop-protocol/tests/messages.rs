#![allow(clippy::unwrap_used)]
use std::num::NonZeroU64;
use td_desktop_protocol::{
    Output, Position, Reply, Request, MAX_PIXELS, MAX_REPLY, MAX_REQUEST, MAX_TEXT,
};

const OUTPUT: Output = Output(NonZeroU64::MIN);
const AT: Position = Position {
    output: OUTPUT,
    x: 300,
    y: 200,
};

#[test]
fn input_transitions_and_clipboard_round_trip() {
    for request in [
        Request::Frame,
        Request::Key {
            code: 42,
            down: true,
        },
        Request::Key {
            code: 42,
            down: false,
        },
        Request::Pointer { at: AT, buttons: 7 },
        Request::Scroll {
            at: AT,
            x: -2,
            y: 1,
        },
        Request::ReleaseAll,
        Request::ReadText,
        Request::WriteText("two\n行"),
        Request::WriteText(""),
    ] {
        let mut message = Vec::new();
        request.write(&mut message).unwrap();
        assert_eq!(Request::decode(&message).unwrap(), request);
        if !matches!(request, Request::WriteText(_)) {
            for length in 0..message.len() {
                assert!(Request::decode(message.get(..length).unwrap()).is_err());
            }
            message.push(0);
            assert!(Request::decode(&message).is_err());
        }
    }
}

#[test]
fn frame_extent_and_payload_must_agree_exactly() {
    let pixels = [0xd2; 24];
    let frame = Reply::Frame {
        output: OUTPUT,
        width: 3,
        height: 2,
        pixels: &pixels,
    };
    let mut message = Vec::new();
    frame.write(&mut message).unwrap();
    assert_eq!(Reply::decode(&message).unwrap(), frame);
    for length in 0..message.len() {
        assert!(Reply::decode(message.get(..length).unwrap()).is_err());
    }
    message.push(0);
    assert!(Reply::decode(&message).is_err());
    for (width, height) in [(3, 1), (0, 0), (u16::MAX, u16::MAX)] {
        let bad = Reply::Frame {
            output: OUTPUT,
            width,
            height,
            pixels: &pixels,
        };
        let mut dst = Vec::new();
        assert!(bad.write(&mut dst).is_err());
        assert!(dst.is_empty());
    }
    assert_eq!(MAX_REPLY, 13 + 4 * MAX_PIXELS);
}

#[test]
fn text_limit_is_in_bytes_and_invalid_utf8_is_refused() {
    let mut text = "é".repeat(MAX_TEXT / 2);
    let mut message = Vec::new();
    Request::WriteText(&text).write(&mut message).unwrap();
    assert_eq!(message.len(), MAX_REQUEST);
    assert_eq!(
        Request::decode(&message).unwrap(),
        Request::WriteText(&text)
    );
    text.push('x');
    message.push(b'x');
    assert!(Request::decode(&message).is_err());
    message.clear();
    assert!(Request::WriteText(&text).write(&mut message).is_err());
    assert!(Reply::Text(&text).write(&mut message).is_err());
    assert!(message.is_empty());
    assert!(Request::decode(&[7, 0xff]).is_err());
    assert!(Reply::decode(&[4, 0xff]).is_err());
}

#[test]
fn reply_status_text_and_unknown_discriminants() {
    for reply in [
        Reply::Applied,
        Reply::Unavailable,
        Reply::StaleOutput,
        Reply::Text("plain\ntext"),
    ] {
        let mut bytes = Vec::new();
        reply.write(&mut bytes).unwrap();
        assert_eq!(Reply::decode(&bytes).unwrap(), reply);
    }
    for bytes in [&[0u8][..], &[255], &[1, 0]] {
        assert!(Request::decode(bytes).is_err());
        assert!(Reply::decode(bytes).is_err());
    }
    let mut message = Vec::new();
    assert!(Request::Pointer { at: AT, buttons: 8 }
        .write(&mut message)
        .is_err());
    assert!(message.is_empty());
    assert!(Request::decode(&[2, 0, 42, 2]).is_err());
    assert!(Request::decode(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]).is_err());
}
