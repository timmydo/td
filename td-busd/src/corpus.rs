//! Committed byte streams, their expected decode, and the encodings that must
//! be refused — plus the self-check that runs all of it.
//!
//! Round-tripping the crate's own writer through its own reader proves the two
//! AGREE, which a codec that is wrong in one consistent way also does. So the
//! streams below are hand-laid against the specification rather than produced
//! by this crate, and the expected decode beside each one is checkable by eye
//! against the bytes. They are what makes the round-trip mean something.
//!
//! The same table is what the shipped binary's `selftest` runs, so the target
//! build decodes exactly what the host tests decode.

use crate::message::{self, Message, MessageError, MessageType};
use crate::wire::{self, Endian, Limits, WireError, Writer};

/// One hand-laid encoding of a body, in both byte orders.
pub struct Stream {
    pub name: &'static str,
    pub signature: &'static str,
    pub little: &'static [u8],
    pub big: &'static [u8],
    /// `render` of each top-level value, joined with `|`.
    pub expect: &'static str,
    /// Writes the same values, so the marshaller is held to the hand-laid bytes
    /// rather than only to this crate's own reader.
    pub encode: fn(&mut Writer) -> Result<(), WireError>,
}

/// One encoding that must be refused, and the refusal it must produce.
///
/// §I requires malformed bodies in BOTH endian modes. `big` is the big-endian
/// spelling of a fixture whose bytes would otherwise mean something else read
/// the other way round: a `s` whose length is `01 00 00 00` is one byte
/// little-endian and 16 megabytes big-endian, which is a different refusal
/// about a different thing.
pub struct Refusal {
    pub name: &'static str,
    pub signature: &'static str,
    pub bytes: &'static [u8],
    /// `None` asserts that `bytes` earns the SAME refusal in both orders —
    /// which running them in both is what checks. That is weaker than "carries
    /// no multibyte integer" and is the property actually wanted: `ab` holding
    /// `02 00 00 00` is 2 one way and 33554432 the other, and neither is a
    /// normal boolean.
    pub big: Option<&'static [u8]>,
    pub error: WireError,
}

/// How the sampler's descriptor index is bounded: three arrived, index 2 is the
/// last legal one.
pub const SAMPLER_FDS: Limits = Limits { fds: 3 };

/// Every type the grammar has, in one body.
pub const SAMPLER_SIGNATURE: &str = "ybnqiuxtdsoghayai(is)a{sv}v";

/// What `SAMPLER_SIGNATURE` must decode to, whichever byte order carried it.
pub const SAMPLER_EXPECT: &str = "y:0x2a|b:true|n:-2|q:0x3|i:-4|u:0x5|x:-6|t:0x7|\
d:0x3fe0000000000000|s:hello|o:/org/freedesktop/DBus|g:a{sv}|h:0x2|ay:dead|\
a[i:1,i:2,i:3]|r[i:9,s:nine]|a[e[s:k,v[u:0xb]]]|v[s:inner]";

pub const STREAMS: &[Stream] = &[
    // `y` at 0; `u` padded to 4; `s` already aligned at 8, a u32 length, the
    // bytes, and a NUL.
    Stream {
        name: "scalars and a string",
        signature: "yus",
        little: &[
            0x2a, 0x00, 0x00, 0x00, 0xef, 0xbe, 0xad, 0xde, 0x05, 0x00, 0x00, 0x00, b'h', b'e',
            b'l', b'l', b'o', 0x00,
        ],
        big: &[
            0x2a, 0x00, 0x00, 0x00, 0xde, 0xad, 0xbe, 0xef, 0x00, 0x00, 0x00, 0x05, b'h', b'e',
            b'l', b'l', b'o', 0x00,
        ],
        expect: "y:0x2a|u:0xdeadbeef|s:hello",
        encode: |w| {
            w.byte(0x2a);
            w.uint32(0xdead_beef);
            w.string("hello")
        },
    },
    // The array rule, in the smallest body that shows it: the declared length
    // is 16 — two `t`s — while the buffer is 24, because the four bytes of
    // padding between the length and the first 8-aligned element are NOT
    // counted in it. A reader that counted them would look for elements at 4.
    Stream {
        name: "array padding outside the declared length",
        signature: "at",
        little: &[
            0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33,
            0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa, 0x99,
        ],
        big: &[
            0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
        ],
        expect: "a[t:0x1122334455667788,t:0x99aabbccddeeff00]",
        encode: |w| {
            w.array("t", |w| {
                w.uint64(0x1122_3344_5566_7788);
                w.uint64(0x99aa_bbcc_ddee_ff00);
                Ok(())
            })
        },
    },
    // `a{sv}`, the shape every property bag arrives in: an 8-aligned dict entry
    // after the same outside-the-length padding, a `g`-framed variant signature
    // that is a u8 length rather than a u32, and the variant's own value padded
    // to its own alignment inside the entry.
    Stream {
        name: "a property dictionary",
        signature: "a{sv}",
        little: &[
            0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, b'a', 0x00,
            0x01, b'u', 0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00,
        ],
        big: &[
            0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, b'a', 0x00,
            0x01, b'u', 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07,
        ],
        expect: "a[e[s:a,v[u:0x7]]]",
        encode: |w| {
            w.array("{sv}", |w| {
                w.dict_entry(|w| {
                    w.string("a")?;
                    w.variant("u", |w| {
                        w.uint32(7);
                        Ok(())
                    })
                })
            })
        },
    },
];

/// One hand-laid message, in both byte orders.
pub struct MessageStream {
    pub name: &'static str,
    pub little: &'static [u8],
    pub big: &'static [u8],
    /// Must reproduce those bytes exactly.
    pub build: fn(Endian) -> Result<Vec<u8>, MessageError>,
    /// `describe` of the decoded message.
    pub expect: &'static str,
}

/// `org.freedesktop.DBus.Hello`, the first message every client sends.
///
/// Laid out by hand from the specification: a 16-byte fixed header whose last
/// word is the `a(yv)` length, then PATH, INTERFACE, MEMBER and DESTINATION at
/// 16, 48, 80 and 96 — each 8-aligned, which is where the 2, 3 and 2 bytes of
/// inter-field padding come from. The fields end at 125 and the header is
/// padded to 128 even though the body is empty.
const HELLO_LITTLE: &[u8] = b"l\x01\x00\x01\x00\x00\x00\x00\x01\x00\x00\x00\x6d\x00\x00\x00\
\x01\x01o\x00\x15\x00\x00\x00/org/freedesktop/DBus\x00\
\x00\x00\
\x02\x01s\x00\x14\x00\x00\x00org.freedesktop.DBus\x00\
\x00\x00\x00\
\x03\x01s\x00\x05\x00\x00\x00Hello\x00\
\x00\x00\
\x06\x01s\x00\x14\x00\x00\x00org.freedesktop.DBus\x00\
\x00\x00\x00";

const HELLO_BIG: &[u8] = b"B\x01\x00\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x6d\
\x01\x01o\x00\x00\x00\x00\x15/org/freedesktop/DBus\x00\
\x00\x00\
\x02\x01s\x00\x00\x00\x00\x14org.freedesktop.DBus\x00\
\x00\x00\x00\
\x03\x01s\x00\x00\x00\x00\x05Hello\x00\
\x00\x00\
\x06\x01s\x00\x00\x00\x00\x14org.freedesktop.DBus\x00\
\x00\x00\x00";

/// The broker's reply to it: the unique name, as a `s` body.
///
/// REPLY_SERIAL, DESTINATION, SENDER and SIGNATURE at 16, 24, 40 and 72. The
/// SIGNATURE field is where the two string framings sit side by side — a `g`
/// is a u8 length and a `s` is a u32 one — and the header pads by a single
/// byte to put the body at 80.
const HELLO_REPLY_LITTLE: &[u8] = b"l\x02\x01\x01\x09\x00\x00\x00\x01\x00\x00\x00\x3f\x00\x00\x00\
\x05\x01u\x00\x01\x00\x00\x00\
\x06\x01s\x00\x04\x00\x00\x00:1.0\x00\
\x00\x00\x00\
\x07\x01s\x00\x14\x00\x00\x00org.freedesktop.DBus\x00\
\x00\x00\x00\
\x08\x01g\x00\x01s\x00\
\x00\
\x04\x00\x00\x00:1.0\x00";

const HELLO_REPLY_BIG: &[u8] = b"B\x02\x01\x01\x00\x00\x00\x09\x00\x00\x00\x01\x00\x00\x00\x3f\
\x05\x01u\x00\x00\x00\x00\x01\
\x06\x01s\x00\x00\x00\x00\x04:1.0\x00\
\x00\x00\x00\
\x07\x01s\x00\x00\x00\x00\x14org.freedesktop.DBus\x00\
\x00\x00\x00\
\x08\x01g\x00\x01s\x00\
\x00\
\x00\x00\x00\x04:1.0\x00";

pub const MESSAGES: &[MessageStream] = &[
    MessageStream {
        name: "the Hello method call",
        little: HELLO_LITTLE,
        big: HELLO_BIG,
        build: |endian| {
            message::Builder::method_call(
                endian,
                "/org/freedesktop/DBus",
                Some("org.freedesktop.DBus"),
                "Hello",
            )
            .destination("org.freedesktop.DBus")
            .serial(1)
            .encode()
        },
        expect: "method_call serial=1 flags=0x0 path=/org/freedesktop/DBus \
interface=org.freedesktop.DBus member=Hello destination=org.freedesktop.DBus",
    },
    MessageStream {
        name: "the Hello reply",
        little: HELLO_REPLY_LITTLE,
        big: HELLO_REPLY_BIG,
        build: |endian| {
            message::Builder::method_return(endian, 1)
                .destination(":1.0")
                .sender("org.freedesktop.DBus")
                .body("s", |w| w.string(":1.0"))?
                .serial(1)
                .encode()
        },
        expect: "method_return serial=1 flags=0x1 destination=:1.0 \
sender=org.freedesktop.DBus signature=s reply_serial=1 args=s::1.0",
    },
];

/// A refusal expressed as a change to `HELLO_LITTLE`, so each one is one edit
/// away from a message that decodes — which is what makes it a test of the
/// check rather than of the rest of the parse.
///
/// The offsets are the hand-laid ones above. They cannot drift silently: the
/// committed bytes are compared against the builder's output before any of
/// these run.
pub struct Mutation {
    pub name: &'static str,
    pub apply: fn(&mut Vec<u8>, Endian),
    pub error: MessageError,
}

/// PATH's field-code byte, the first byte of the first field struct.
const PATH_CODE_AT: usize = 16;
/// The `o` in PATH's variant signature.
const PATH_VARIANT_SIGNATURE_AT: usize = 18;
/// The first of the three padding bytes between the fields and the body.
const PRE_BODY_PADDING_AT: usize = 125;
/// The last byte of INTERFACE's value.
const INTERFACE_LAST_AT: usize = 75;
/// MEMBER's field-code byte.
const MEMBER_CODE_AT: usize = 80;
/// The first byte of MEMBER's value: its code at 80, `\x01 s \0`, then a u32
/// length, so `Hello` begins at 88.
const MEMBER_VALUE_AT: usize = MEMBER_CODE_AT + 8;
/// The first byte of DESTINATION's value, by the same reckoning.
const DESTINATION_VALUE_AT: usize = DESTINATION_CODE_AT + 8;
/// DESTINATION's field-code byte.
const DESTINATION_CODE_AT: usize = 96;

fn poke(bytes: &mut [u8], at: usize, value: u8) {
    if let Some(slot) = bytes.get_mut(at) {
        *slot = value;
    }
}

fn poke_u32(bytes: &mut [u8], at: usize, value: u32, endian: Endian) {
    let Some(end) = at.checked_add(4) else { return };
    if let Some(slot) = bytes.get_mut(at..end) {
        slot.copy_from_slice(&match endian {
            Endian::Little => value.to_le_bytes(),
            Endian::Big => value.to_be_bytes(),
        });
    }
}

pub const MUTATIONS: &[Mutation] = &[
    Mutation {
        name: "an endianness byte that is neither l nor B",
        apply: |bytes, _endian| poke(bytes, 0, b'x'),
        error: MessageError::BadEndianness(b'x'),
    },
    Mutation {
        name: "message type 0, which is INVALID",
        apply: |bytes, _endian| poke(bytes, 1, 0),
        error: MessageError::InvalidType,
    },
    Mutation {
        name: "a protocol version that is not 1",
        apply: |bytes, _endian| poke(bytes, 3, 2),
        error: MessageError::UnsupportedVersion(2),
    },
    Mutation {
        name: "a zero serial",
        apply: |bytes, endian| poke_u32(bytes, 8, 0, endian),
        error: MessageError::ZeroSerial,
    },
    Mutation {
        name: "a body length over the ceiling",
        apply: |bytes, endian| poke_u32(bytes, 4, message::MAX_BODY_BYTES + 1, endian),
        error: MessageError::BodyTooLarge,
    },
    Mutation {
        name: "header fields over the ceiling",
        apply: |bytes, endian| poke_u32(bytes, 12, message::MAX_HEADER_FIELDS_BYTES + 1, endian),
        error: MessageError::HeaderFieldsTooLarge,
    },
    Mutation {
        name: "a frame shorter than its header declares",
        apply: |bytes, _endian| {
            bytes.pop();
        },
        error: MessageError::Truncated,
    },
    Mutation {
        name: "a body with no SIGNATURE field",
        apply: |bytes, endian| {
            poke_u32(bytes, 4, 4, endian);
            bytes.extend_from_slice(&[0, 0, 0, 0]);
        },
        error: MessageError::BodyWithoutSignature,
    },
    // PATH's value is a perfectly good string; what is wrong is that it did not
    // go through the object-path grammar, which is exactly what the type check
    // is for.
    Mutation {
        name: "a PATH field carrying a plain string",
        apply: |bytes, _endian| poke(bytes, PATH_VARIANT_SIGNATURE_AT, b's'),
        error: MessageError::FieldTypeMismatch(1),
    },
    Mutation {
        name: "an interface name ending in a dot",
        apply: |bytes, _endian| poke(bytes, INTERFACE_LAST_AT, b'.'),
        error: MessageError::BadInterfaceName,
    },
    Mutation {
        name: "a member name with a dot in it",
        apply: |bytes, _endian| poke(bytes, MEMBER_VALUE_AT + 3, b'.'),
        error: MessageError::BadMemberName,
    },
    Mutation {
        name: "a second MEMBER field",
        apply: |bytes, _endian| poke(bytes, DESTINATION_CODE_AT, 3),
        error: MessageError::DuplicateField(3),
    },
    // Field code 200 is unknown, so it is IGNORED rather than refused — which
    // leaves the call with no MEMBER at all.
    Mutation {
        name: "a method call whose MEMBER became an unknown field",
        apply: |bytes, _endian| poke(bytes, MEMBER_CODE_AT, 200),
        error: MessageError::MissingField("MEMBER"),
    },
    // A `s` where UNIX_FDS must be a `u`: the code decides the type, so
    // relabelling a field is a type mismatch rather than a reinterpretation.
    Mutation {
        name: "a UNIX_FDS field carrying a string",
        apply: |bytes, _endian| poke(bytes, MEMBER_CODE_AT, 9),
        error: MessageError::FieldTypeMismatch(9),
    },
    // The other two names `check_names` applies. They run through `encode` in
    // the host tests, but the SHIPPED selftest never saw a malformed
    // DESTINATION or ERROR_NAME until these, and both are one edit from a
    // message that decodes like the rest of the table.
    Mutation {
        name: "a DESTINATION whose first element begins with a digit",
        apply: |bytes, _endian| poke(bytes, DESTINATION_VALUE_AT, b'9'),
        error: MessageError::BadBusName,
    },
    // MEMBER and ERROR_NAME are both `s`, so relabelling the code is all it
    // takes — and `Hello` is one element, which an error name may not be.
    Mutation {
        name: "a MEMBER relabelled ERROR_NAME, which needs two elements",
        apply: |bytes, _endian| poke(bytes, MEMBER_CODE_AT, 4),
        error: MessageError::BadErrorName,
    },
    // The fields end at 125 and the body is 8-aligned at 128, so these three
    // bytes are the padding no `Reader::align` ever walks over.
    Mutation {
        name: "a non-zero byte in the padding before the body",
        apply: |bytes, _endian| poke(bytes, PRE_BODY_PADDING_AT, 0xff),
        error: MessageError::Wire(WireError::NonZeroPadding),
    },
    // 0 is INVALID rather than unknown: it is the one code the specification
    // says cannot appear, so it is refused where 200 above is ignored.
    Mutation {
        name: "a header field whose code is 0",
        apply: |bytes, _endian| poke(bytes, PATH_CODE_AT, 0),
        error: MessageError::InvalidFieldCode,
    },
];

/// Little-endian, since the refusal never depends on the byte order.
pub const REFUSALS: &[Refusal] = &[
    Refusal {
        name: "non-zero alignment padding",
        signature: "yu",
        bytes: &[0x01, 0xff, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00],
        big: None,
        error: WireError::NonZeroPadding,
    },
    Refusal {
        name: "a boolean that is neither 0 nor 1",
        signature: "b",
        bytes: &[0x02, 0x00, 0x00, 0x00],
        big: None,
        error: WireError::NonNormalBool,
    },
    Refusal {
        name: "a string that is not UTF-8",
        signature: "s",
        bytes: &[0x01, 0x00, 0x00, 0x00, 0xff, 0x00],
        big: Some(&[0x00, 0x00, 0x00, 0x01, 0xff, 0x00]),
        error: WireError::BadUtf8,
    },
    Refusal {
        name: "a string with an interior NUL",
        signature: "s",
        bytes: &[0x03, 0x00, 0x00, 0x00, b'a', 0x00, b'b', 0x00],
        big: Some(&[0x00, 0x00, 0x00, 0x03, b'a', 0x00, b'b', 0x00]),
        error: WireError::InteriorNul,
    },
    Refusal {
        name: "a string with no terminator",
        signature: "s",
        bytes: &[0x01, 0x00, 0x00, 0x00, b'a'],
        big: Some(&[0x00, 0x00, 0x00, 0x01, b'a']),
        error: WireError::Truncated,
    },
    Refusal {
        name: "an object path that is not one",
        signature: "o",
        bytes: &[0x03, 0x00, 0x00, 0x00, b'a', b'b', b'c', 0x00],
        big: Some(&[0x00, 0x00, 0x00, 0x03, b'a', b'b', b'c', 0x00]),
        error: WireError::BadObjectPath,
    },
    Refusal {
        name: "an array whose elements overrun its declared length",
        signature: "ai",
        bytes: &[0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        big: Some(&[0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00]),
        error: WireError::ArrayLengthMismatch,
    },
    Refusal {
        name: "an array declaring more than 2^26 bytes",
        signature: "ay",
        bytes: &[0x01, 0x00, 0x00, 0x04],
        big: Some(&[0x04, 0x00, 0x00, 0x01]),
        error: WireError::ArrayLengthTooLarge,
    },
    Refusal {
        name: "a fixed value cut short",
        signature: "u",
        bytes: &[0x01, 0x02, 0x03],
        big: None,
        error: WireError::Truncated,
    },
    Refusal {
        name: "a body longer than its signature",
        signature: "y",
        bytes: &[0x01, 0x02],
        big: None,
        error: WireError::TrailingBytes,
    },
    // The count says how many descriptors arrived; it says nothing about
    // whether index 7 exists when three did, and this body claims index 0 of
    // none.
    Refusal {
        name: "a descriptor index naming a descriptor that did not arrive",
        signature: "h",
        bytes: &[0x00, 0x00, 0x00, 0x00],
        big: None,
        error: WireError::FdIndexOutOfRange,
    },
    Refusal {
        name: "a reserved type code",
        signature: "m",
        bytes: &[],
        big: None,
        error: WireError::ReservedTypeCode,
    },
    Refusal {
        name: "a dict entry outside an array",
        signature: "{sv}",
        bytes: &[],
        big: None,
        error: WireError::BadDictEntry,
    },
    Refusal {
        name: "a struct with no fields",
        signature: "()",
        bytes: &[],
        big: None,
        error: WireError::EmptyStruct,
    },
    Refusal {
        name: "a dict entry keyed by a container",
        signature: "a{vs}",
        bytes: &[],
        big: None,
        error: WireError::NonBasicDictKey,
    },
    Refusal {
        name: "an unclosed struct",
        signature: "(i",
        bytes: &[],
        big: None,
        error: WireError::BadSignature,
    },
    Refusal {
        name: "a variant declaring no type",
        signature: "v",
        bytes: &[0x00, 0x00],
        big: None,
        error: WireError::BadSignature,
    },
    // `ab` and `ah` are the two arrays the fixed-width fast path must NOT
    // take: each element carries a per-value check, and skipping the walk
    // would carry a non-normal boolean or a descriptor index naming nothing
    // across the decode boundary.
    Refusal {
        name: "an array of booleans holding one that is neither 0 nor 1",
        signature: "ab",
        bytes: &[0x04, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00],
        big: Some(&[0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x02]),
        error: WireError::NonNormalBool,
    },
    Refusal {
        name: "an array of descriptor indices naming one that did not arrive",
        signature: "ah",
        bytes: &[0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        // The declared length is the multibyte integer here: `04 00 00 00` read
        // big-endian is 64 MiB, which runs past the buffer long before any
        // index is looked at.
        big: Some(&[0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00]),
        error: WireError::FdIndexOutOfRange,
    },
    Refusal {
        name: "a variant declaring two types",
        signature: "v",
        bytes: &[0x02, b'y', b'y', 0x00, 0x01, 0x02],
        big: None,
        error: WireError::BadSignature,
    },
];

/// Write one value of every type the grammar has.
pub fn write_sampler(w: &mut Writer) -> Result<(), WireError> {
    w.byte(0x2a);
    w.bool(true);
    w.int16(-2);
    w.uint16(3);
    w.int32(-4);
    w.uint32(5);
    w.int64(-6);
    w.uint64(7);
    w.double(0.5);
    w.string("hello")?;
    w.object_path("/org/freedesktop/DBus")?;
    w.signature("a{sv}")?;
    w.unix_fd(2);
    w.array("y", |w| {
        w.byte(0xde);
        w.byte(0xad);
        Ok(())
    })?;
    w.array("i", |w| {
        w.int32(1);
        w.int32(2);
        w.int32(3);
        Ok(())
    })?;
    w.structure(|w| {
        w.int32(9);
        w.string("nine")
    })?;
    w.array("{sv}", |w| {
        w.dict_entry(|w| {
            w.string("k")?;
            w.variant("u", |w| {
                w.uint32(11);
                Ok(())
            })
        })
    })?;
    w.variant("s", |w| w.string("inner"))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn decode(bytes: &[u8], signature: &str, endian: Endian, limits: Limits) -> Result<String, String> {
    let values = wire::read_body(bytes, signature, endian, limits)
        .map_err(|e| format!("decoding {signature} failed: {e}"))?;
    let mut rendered = Vec::new();
    for value in &values {
        rendered.push(wire::render(value).map_err(|e| format!("rendering {signature}: {e}"))?);
    }
    Ok(rendered.join("|"))
}

/// A decoded message as one deterministic line, so a committed expectation can
/// be read beside the committed bytes it comes from.
pub fn describe(message: &Message<'_>) -> Result<String, String> {
    let kind = match message.kind {
        MessageType::MethodCall => "method_call".to_string(),
        MessageType::MethodReturn => "method_return".to_string(),
        MessageType::Error => "error".to_string(),
        MessageType::Signal => "signal".to_string(),
        MessageType::Unknown(code) => format!("unknown({code})"),
    };
    let mut parts = vec![
        kind,
        format!("serial={}", message.serial),
        format!("flags={:#x}", message.flags),
    ];
    let fields = &message.fields;
    for (label, value) in [
        ("path", fields.path),
        ("interface", fields.interface),
        ("member", fields.member),
        ("error_name", fields.error_name),
        ("destination", fields.destination),
        ("sender", fields.sender),
        ("signature", fields.signature),
    ] {
        if let Some(value) = value {
            parts.push(format!("{label}={value}"));
        }
    }
    for (label, value) in [
        ("reply_serial", fields.reply_serial),
        ("unix_fds", fields.unix_fds),
    ] {
        if let Some(value) = value {
            parts.push(format!("{label}={value}"));
        }
    }
    if !message.args().is_empty() {
        let mut rendered = Vec::new();
        for value in message.args() {
            rendered.push(wire::render(value).map_err(|e| format!("rendering an argument: {e}"))?);
        }
        parts.push(format!("args={}", rendered.join("|")));
    }
    Ok(parts.join(" "))
}

fn check_messages() -> Result<(), String> {
    for stream in MESSAGES {
        for (endian, bytes) in [(Endian::Little, stream.little), (Endian::Big, stream.big)] {
            let (message, consumed) = message::decode(bytes, 0)
                .map_err(|e| format!("{}: decoding failed: {e}", stream.name))?;
            if consumed != bytes.len() {
                return Err(format!(
                    "{}: decoded {consumed} bytes of a {}-byte message",
                    stream.name,
                    bytes.len()
                ));
            }
            let described = describe(&message).map_err(|e| format!("{}: {e}", stream.name))?;
            if described != stream.expect {
                return Err(format!(
                    "{}: decoded differently from its committed expectation\n  expected: {}\n  decoded:  {described}",
                    stream.name, stream.expect
                ));
            }
            let built = (stream.build)(endian)
                .map_err(|e| format!("{}: encoding failed: {e}", stream.name))?;
            if built != bytes {
                return Err(format!(
                    "{}: encoded bytes differ from the committed message\n  expected: {}\n  produced: {}",
                    stream.name,
                    hex(bytes),
                    hex(&built)
                ));
            }
        }
    }

    // Both byte orders, because a header check reading a length the wrong way
    // round — or a ceiling applied on one path only — would pass every refusal
    // run against the little-endian stream alone. The offsets are shared: the
    // two streams differ in their integer encodings, not in their layout.
    for mutation in MUTATIONS {
        for (endian, stream) in [(Endian::Little, HELLO_LITTLE), (Endian::Big, HELLO_BIG)] {
            let mut bytes = stream.to_vec();
            (mutation.apply)(&mut bytes, endian);
            match message::decode(&bytes, 0) {
                Ok(_) => {
                    return Err(format!(
                        "{} ({endian:?}): was accepted and must not be",
                        mutation.name
                    ))
                }
                Err(error) if error == mutation.error => {}
                Err(error) => {
                    return Err(format!(
                        "{} ({endian:?}): refused with {error:?} rather than {:?}",
                        mutation.name, mutation.error
                    ))
                }
            }
        }
    }

    // The declared count is a claim about what accompanies the message, so it
    // is checked against what actually arrived rather than trusted.
    match message::decode(HELLO_LITTLE, 1) {
        Err(MessageError::FdCountMismatch) => {}
        other => {
            return Err(format!(
                "a message declaring no descriptors was accepted alongside one: {other:?}"
            ))
        }
    }

    // Zero is refused as a message serial, so a reply naming it names a call no
    // peer can ever have made. The decode side has no committed stream to
    // mutate — neither hand-laid message carries a REPLY_SERIAL of zero,
    // because one cannot be built — so the builder is where it is asked.
    match message::Builder::method_return(Endian::Little, 0)
        .serial(1)
        .encode()
    {
        Err(MessageError::ZeroReplySerial) => {}
        other => return Err(format!("a reply to serial zero was built: {other:?}")),
    }

    // Everything below goes through `decode_from_client`, which is the entry
    // point a transport should reach for: `decode` alone leaves both identity
    // refusals to a second call a caller can forget with no compile error.
    // The reply is well-formed and still refused — the broker inserts SENDER,
    // so a client that sets one is claiming to be somebody.
    match message::decode_from_client(HELLO_REPLY_LITTLE, 0) {
        Err(MessageError::SenderFromClient) => {}
        other => {
            return Err(format!(
                "a client-supplied SENDER was accepted: {}",
                match other {
                    Ok(_) => "it decoded".to_string(),
                    Err(e) => format!("{e}"),
                }
            ))
        }
    }
    message::decode_from_client(HELLO_LITTLE, 0)
        .map_err(|e| format!("a call with no SENDER was refused: {e}"))?;

    // The bus's own local path and interface are not addresses a client may
    // use: a client that could send `Disconnected` would be forging a message
    // from the bus about another client's connection.
    let forged = message::Builder::signal(
        Endian::Little,
        message::LOCAL_PATH,
        message::LOCAL_INTERFACE,
        "Disconnected",
    )
    .serial(1)
    .encode()
    .map_err(|e| format!("encoding the local signal: {e}"))?;
    match message::decode_from_client(&forged, 0) {
        Err(MessageError::ReservedLocalName) => {}
        other => {
            return Err(format!(
                "a client-sent local signal was accepted: {}",
                match other {
                    Ok(_) => "it decoded".to_string(),
                    Err(e) => format!("{e}"),
                }
            ))
        }
    }
    // ...while `decode` alone still accepts it, because the BUS sends exactly
    // this message and must be able to marshal and read it back.
    message::decode(&forged, 0)
        .map_err(|e| format!("the bus's own local signal was refused: {e}"))?;

    // A stream reader needs the frame length before it has the frame.
    match message::frame_len(HELLO_LITTLE) {
        Ok(Some(len)) if len == HELLO_LITTLE.len() => {}
        other => return Err(format!("frame_len disagreed with the message: {other:?}")),
    }
    match message::frame_len(&[]) {
        Ok(None) => {}
        other => return Err(format!("frame_len claimed to know an empty frame: {other:?}")),
    }

    check_round_trips()
}

/// The two message types the committed streams do not carry, plus the
/// descriptor count, through encode and back.
///
/// These are round-trips rather than hand-laid bytes: the FRAMING is already
/// pinned byte for byte by the two streams above, so what is left to check here
/// is the per-type mandatory-field rule and the fields only these carry.
fn check_round_trips() -> Result<(), String> {
    for endian in [Endian::Little, Endian::Big] {
        let signal = message::Builder::signal(
            endian,
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameOwnerChanged",
        )
        .serial(7)
        .body("sss", |w| {
            w.string("org.example.App")?;
            w.string("")?;
            w.string(":1.4")
        })
        .map_err(|e| format!("marshalling NameOwnerChanged: {e}"))?
        .encode()
        .map_err(|e| format!("encoding NameOwnerChanged: {e}"))?;
        let (decoded, _) = message::decode(&signal, 0)
            .map_err(|e| format!("decoding NameOwnerChanged: {e}"))?;
        let expected = "signal serial=7 flags=0x0 path=/org/freedesktop/DBus \
interface=org.freedesktop.DBus member=NameOwnerChanged signature=sss \
args=s:org.example.App|s:|s::1.4";
        let described = describe(&decoded)?;
        if described != expected {
            return Err(format!(
                "NameOwnerChanged did not survive a round trip\n  expected: {expected}\n  decoded:  {described}"
            ));
        }
        // The two walks are independent: one counts complete types in the
        // signature, the other reads values out of the body.
        let arity = wire::signature_arity("sss")
            .map_err(|e| format!("counting the signature's types: {e}"))?;
        if arity != decoded.args().len() {
            return Err(format!(
                "the signature declares {arity} arguments and the body carried {}",
                decoded.args().len()
            ));
        }

        let error = message::Builder::error(endian, "org.freedesktop.DBus.Error.AccessDenied", 7)
            .destination(":1.4")
            .serial(8)
            .body("s", |w| w.string("no"))
            .map_err(|e| format!("marshalling the error: {e}"))?
            .encode()
            .map_err(|e| format!("encoding the error: {e}"))?;
        let (decoded, _) =
            message::decode(&error, 0).map_err(|e| format!("decoding the error: {e}"))?;
        let expected = "error serial=8 flags=0x1 error_name=org.freedesktop.DBus.Error.AccessDenied \
destination=:1.4 signature=s reply_serial=7 args=s:no";
        let described = describe(&decoded)?;
        if described != expected {
            return Err(format!(
                "the error did not survive a round trip\n  expected: {expected}\n  decoded:  {described}"
            ));
        }
    }

    // A message that says two descriptors accompany it decodes when two do, and
    // its `h` arguments index them.
    let carrier = message::Builder::method_call(
        Endian::Little,
        "/org/example",
        Some("org.example.Sink"),
        "Take",
    )
    .unix_fds(2)
    .flags(message::FLAG_NO_REPLY_EXPECTED)
    .serial(9)
    .body("hh", |w| {
        w.unix_fd(0);
        w.unix_fd(1);
        Ok(())
    })
    .map_err(|e| format!("marshalling the descriptor carrier: {e}"))?
    .encode()
    .map_err(|e| format!("encoding the descriptor carrier: {e}"))?;
    match message::decode(&carrier, 2) {
        Ok((decoded, _)) if decoded.fields.unix_fds == Some(2) => {}
        other => return Err(format!("a two-descriptor message did not decode: {other:?}")),
    }
    match message::decode(&carrier, 1) {
        Err(MessageError::FdCountMismatch) => {}
        other => {
            return Err(format!(
                "a message claiming two descriptors was accepted with one: {other:?}"
            ))
        }
    }

    match message::Builder::method_call(
        Endian::Little,
        "/org/example",
        Some("org.example.Sink"),
        "Take",
    )
        .unix_fds(message::MAX_FDS_PER_MESSAGE + 1)
        .serial(10)
        .encode()
    {
        Err(MessageError::TooManyFds) => {}
        other => return Err(format!("a message over the descriptor cap was built: {other:?}")),
    }
    Ok(())
}

/// Round-trip the sampler through both byte orders, decode every committed
/// stream, and require every committed refusal.
///
/// Returns the one-line summary the binary prints, or the first failure.
pub fn selftest() -> Result<String, String> {
    let mut lengths = Vec::new();
    for endian in [Endian::Little, Endian::Big] {
        let mut writer = Writer::new(endian);
        write_sampler(&mut writer).map_err(|e| format!("marshalling the sampler: {e}"))?;
        let bytes = writer.into_bytes();
        lengths.push(bytes.len());
        let rendered = decode(&bytes, SAMPLER_SIGNATURE, endian, SAMPLER_FDS)?;
        if rendered != SAMPLER_EXPECT {
            return Err(format!(
                "the sampler decoded differently from its committed expectation\n  expected: {SAMPLER_EXPECT}\n  decoded:  {rendered}"
            ));
        }
    }
    // Alignment does not depend on the byte order, so the two encodings differ
    // in their bytes and never in their length.
    if lengths.first() != lengths.get(1) {
        return Err(format!(
            "the two byte orders marshalled the sampler to different lengths: {lengths:?}"
        ));
    }

    for stream in STREAMS {
        for (endian, bytes) in [
            (Endian::Little, stream.little),
            (Endian::Big, stream.big),
        ] {
            let rendered = decode(bytes, stream.signature, endian, SAMPLER_FDS)
                .map_err(|e| format!("{}: {e}", stream.name))?;
            if rendered != stream.expect {
                return Err(format!(
                    "{}: the committed stream decoded differently from its committed expectation\n  expected: {}\n  decoded:  {rendered}",
                    stream.name, stream.expect
                ));
            }
            let mut writer = Writer::new(endian);
            (stream.encode)(&mut writer)
                .map_err(|e| format!("{}: marshalling failed: {e}", stream.name))?;
            if writer.as_bytes() != bytes {
                return Err(format!(
                    "{}: marshalled bytes differ from the committed stream\n  expected: {}\n  produced: {}",
                    stream.name,
                    hex(bytes),
                    hex(writer.as_bytes())
                ));
            }
        }
    }

    for refusal in REFUSALS {
        for (endian, bytes) in [
            (Endian::Little, refusal.bytes),
            (Endian::Big, refusal.big.unwrap_or(refusal.bytes)),
        ] {
            match wire::read_body(bytes, refusal.signature, endian, Limits::NO_FDS) {
                Ok(_) => {
                    return Err(format!(
                        "{} ({endian:?}): was accepted and must not be",
                        refusal.name
                    ));
                }
                Err(error) if error == refusal.error => {}
                Err(error) => {
                    return Err(format!(
                        "{} ({endian:?}): refused with {error:?} rather than {:?}",
                        refusal.name, refusal.error
                    ));
                }
            }
        }
    }

    check_messages()?;

    Ok(format!(
        "td-busd: the D-Bus codec round-trips every type in both byte orders, \
marshals and decodes {} committed bodies and {} committed messages byte for \
byte, and refuses {} malformed bodies and {} malformed messages in each byte \
order",
        STREAMS.len(),
        MESSAGES.len(),
        REFUSALS.len(),
        MUTATIONS.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_selftest_passes() {
        match selftest() {
            Ok(summary) => assert!(summary.contains("committed")),
            Err(failure) => panic!("{failure}"),
        }
    }

    /// The point of the committed streams is that they are NOT this crate's
    /// output, so a codec wrong in one consistent way fails here rather than
    /// round-tripping happily. That only holds while the two byte orders really
    /// are different bytes for the same values.
    #[test]
    fn every_committed_stream_carries_two_distinct_byte_orders() {
        for stream in STREAMS {
            assert_eq!(
                stream.little.len(),
                stream.big.len(),
                "{}: the two byte orders must marshal to one length",
                stream.name
            );
            assert_ne!(
                stream.little, stream.big,
                "{}: the two encodings are identical, so one of them is untested",
                stream.name
            );
        }
    }

    #[test]
    fn the_sampler_covers_every_type_code_in_the_grammar() {
        for code in "ybnqiuxtdsoghav".bytes() {
            assert!(
                SAMPLER_SIGNATURE.as_bytes().contains(&code),
                "the sampler does not carry a {} value",
                char::from(code)
            );
        }
        assert!(SAMPLER_SIGNATURE.contains('('), "no struct in the sampler");
        assert!(SAMPLER_SIGNATURE.contains('{'), "no dict entry in the sampler");
    }

    /// Each refusal must name a distinct failure, or the table would look like
    /// coverage it does not have.
    #[test]
    fn the_refusals_are_not_all_one_error() {
        let mut seen: Vec<WireError> = Vec::new();
        for refusal in REFUSALS {
            if !seen.contains(&refusal.error) {
                seen.push(refusal.error);
            }
        }
        assert!(
            seen.len() >= 12,
            "the refusal table exercises only {} distinct errors",
            seen.len()
        );
    }
}
