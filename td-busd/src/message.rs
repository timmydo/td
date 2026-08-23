//! The D-Bus message: the 16-byte fixed header, the `a(yv)` header fields, and
//! the body at 8-byte alignment.
//!
//! The header-field array is read at position 12 OF THE WHOLE MESSAGE rather
//! than out of a slice of its own, because its `(yv)` elements are 8-aligned
//! against the message's start: a reader handed `&bytes[12..]` would pad four
//! bytes that are not there and look for the first field at 20.
//!
//! Every refusal below disconnects the sender rather than producing an error
//! reply, per `APPLICATIONS.md` §D. That is why they are all here at the
//! decode boundary and none of them is a policy decision.

use std::fmt;

use crate::name;
use crate::wire::{self, Endian, Limits, Value, WireError, Writer};

/// The fixed part: endianness, type, flags, version, body length, serial, and
/// the header-field array's own length.
pub const HEADER_LEN: usize = 16;

pub const PROTOCOL_VERSION: u8 = 1;

/// §D's ceilings. Each is a named constant with a refusal test.
pub const MAX_HEADER_FIELDS_BYTES: u32 = 64 * 1024;
pub const MAX_BODY_BYTES: u32 = 16 * 1024 * 1024;
pub const MAX_FDS_PER_MESSAGE: u32 = 64;

/// The one flag this layer acts on. The other two the specification defines —
/// NO_AUTO_START and ALLOW_INTERACTIVE_AUTHORIZATION — are carried through
/// opaquely in `flags` and arrive as named constants with the rungs that read
/// them, since an unknown flag bit must be ignored rather than refused.
pub const FLAG_NO_REPLY_EXPECTED: u8 = 0x01;

/// The path and interface the specification reserves for messages the BUS
/// generates about a connection itself — `Disconnected` above all.
///
/// They are not addresses a client may use, and the reference implementation
/// disconnects one that tries. Refusing them is a decode-boundary concern
/// rather than a routing one: once routing lands, a client that could send
/// these would be forging a message from the bus about another client's
/// connection, and by then the message is already inside.
pub const LOCAL_PATH: &str = "/org/freedesktop/DBus/Local";
pub const LOCAL_INTERFACE: &str = "org.freedesktop.DBus.Local";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageType {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
    /// The specification says to IGNORE a type this version does not know
    /// rather than to disconnect over it, so one decodes and the broker drops
    /// it. Type 0 is not this: it is INVALID, and it is refused.
    Unknown(u8),
}

impl MessageType {
    fn from_code(code: u8) -> Result<Self, MessageError> {
        Ok(match code {
            0 => return Err(MessageError::InvalidType),
            1 => Self::MethodCall,
            2 => Self::MethodReturn,
            3 => Self::Error,
            4 => Self::Signal,
            other => Self::Unknown(other),
        })
    }

    pub fn code(self) -> u8 {
        match self {
            Self::MethodCall => 1,
            Self::MethodReturn => 2,
            Self::Error => 3,
            Self::Signal => 4,
            Self::Unknown(code) => code,
        }
    }
}

/// The nine header fields this broker knows. An unknown code is ignored, per
/// the specification, but its variant is still parsed: skipping it by length
/// would mean trusting a field this code has never validated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FieldCode {
    Path,
    Interface,
    Member,
    ErrorName,
    ReplySerial,
    Destination,
    Sender,
    Signature,
    UnixFds,
}

impl FieldCode {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Path,
            2 => Self::Interface,
            3 => Self::Member,
            4 => Self::ErrorName,
            5 => Self::ReplySerial,
            6 => Self::Destination,
            7 => Self::Sender,
            8 => Self::Signature,
            9 => Self::UnixFds,
            _ => return None,
        })
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Path => 1,
            Self::Interface => 2,
            Self::Member => 3,
            Self::ErrorName => 4,
            Self::ReplySerial => 5,
            Self::Destination => 6,
            Self::Sender => 7,
            Self::Signature => 8,
            Self::UnixFds => 9,
        }
    }

    /// The variant type this field must carry. A mismatch is a refusal rather
    /// than a coercion: a PATH sent as `s` has not been through the object-path
    /// grammar, and accepting it would route on a string no peer can address.
    pub fn signature(self) -> &'static str {
        match self {
            Self::Path => "o",
            Self::Interface
            | Self::Member
            | Self::ErrorName
            | Self::Destination
            | Self::Sender => "s",
            Self::ReplySerial | Self::UnixFds => "u",
            Self::Signature => "g",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Fields<'a> {
    pub path: Option<&'a str>,
    pub interface: Option<&'a str>,
    pub member: Option<&'a str>,
    pub error_name: Option<&'a str>,
    pub reply_serial: Option<u32>,
    pub destination: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub signature: Option<&'a str>,
    pub unix_fds: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageError {
    Wire(WireError),
    ShortHeader,
    BadEndianness(u8),
    InvalidType,
    UnsupportedVersion(u8),
    ZeroSerial,
    ZeroReplySerial,
    ReservedLocalName,
    InvalidFieldCode,
    BodyTooLarge,
    HeaderFieldsTooLarge,
    Truncated,
    DuplicateField(u8),
    FieldTypeMismatch(u8),
    MissingField(&'static str),
    BodyWithoutSignature,
    SenderFromClient,
    BadInterfaceName,
    BadMemberName,
    BadErrorName,
    BadBusName,
    TooManyFds,
    FdCountMismatch,
}

impl From<WireError> for MessageError {
    fn from(error: WireError) -> Self {
        Self::Wire(error)
    }
}

impl fmt::Display for MessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => write!(f, "{error}"),
            Self::ShortHeader => f.write_str("fewer than 16 bytes of fixed header"),
            Self::BadEndianness(byte) => write!(f, "endianness byte {byte:#x} is neither l nor B"),
            Self::InvalidType => f.write_str("message type 0 is INVALID"),
            Self::UnsupportedVersion(v) => write!(f, "protocol version {v} is not 1"),
            Self::ZeroSerial => f.write_str("a message serial must not be zero"),
            Self::ZeroReplySerial => {
                f.write_str("REPLY_SERIAL names serial zero, which no call can have")
            }
            Self::InvalidFieldCode => f.write_str("header field code 0 is INVALID"),
            Self::ReservedLocalName => {
                f.write_str("a client may not address the bus's own local path or interface")
            }
            Self::BodyTooLarge => f.write_str("the body is over the ceiling"),
            Self::HeaderFieldsTooLarge => f.write_str("the header fields are over the ceiling"),
            Self::Truncated => f.write_str("the message is shorter than its header declares"),
            Self::DuplicateField(code) => write!(f, "header field {code} appears twice"),
            Self::FieldTypeMismatch(code) => {
                write!(f, "header field {code} carries the wrong type")
            }
            Self::MissingField(what) => write!(f, "a mandatory header field is missing: {what}"),
            Self::BodyWithoutSignature => f.write_str("a body arrived with no SIGNATURE field"),
            Self::SenderFromClient => f.write_str("a client may not set SENDER"),
            Self::BadInterfaceName => f.write_str("malformed interface name"),
            Self::BadMemberName => f.write_str("malformed member name"),
            Self::BadErrorName => f.write_str("malformed error name"),
            Self::BadBusName => f.write_str("malformed bus name"),
            Self::TooManyFds => f.write_str("more descriptors than a message may carry"),
            Self::FdCountMismatch => {
                f.write_str("UNIX_FDS disagrees with the descriptors that arrived")
            }
        }
    }
}

/// A decoded message. Its body arguments are decoded with it, because the body
/// has to be walked to know it matches its signature at all — which is a
/// disconnect, not a lazy error.
#[derive(Clone, Debug, PartialEq)]
pub struct Message<'a> {
    pub endian: Endian,
    pub kind: MessageType,
    pub flags: u8,
    pub serial: u32,
    pub fields: Fields<'a>,
    args: Vec<Value<'a>>,
    /// The body exactly as it arrived, still in this message's own byte order.
    ///
    /// A broker forwards a body rather than reading it, and the only faithful
    /// way to do that is to copy the bytes. Re-marshalling the decoded `args`
    /// would work for everything this codec round-trips and would silently
    /// change any encoding the specification permits but this writer does not
    /// choose — a non-minimal padding, a different but legal ordering.
    ///
    /// FIDELITY is the whole argument. A draft of this comment also claimed it
    /// was cheaper, which it is not: `encode` validates the body against its
    /// signature with a full `read_body` walk and then copies it again, so a
    /// relay walks the body twice and copies it twice either way. The second
    /// walk is redundant for a body that was just decoded against the same
    /// signature and byte order, and skipping it is an optimisation with a
    /// trusted-input footgun in it — recorded in `APPLICATIONS.md` §D rather
    /// than taken here.
    body: &'a [u8],
}

impl<'a> Message<'a> {
    pub fn args(&self) -> &[Value<'a>] {
        &self.args
    }

    /// What a CLIENT may not send, as opposed to what is malformed.
    ///
    /// Separate from `decode` because the broker's own outgoing messages are
    /// exactly the ones allowed to do both of these: it inserts the
    /// authenticated SENDER, and it is the only sender of the local
    /// connection signals.
    pub fn check_from_client(&self) -> Result<(), MessageError> {
        if self.fields.sender.is_some() {
            return Err(MessageError::SenderFromClient);
        }
        if self.fields.path == Some(LOCAL_PATH)
            || self.fields.interface == Some(LOCAL_INTERFACE)
        {
            return Err(MessageError::ReservedLocalName);
        }
        Ok(())
    }
}

/// Round `value` up to the next multiple of 8.
fn pad8(value: usize) -> Result<usize, MessageError> {
    value
        .checked_add(7)
        .map(|sum| sum & !7)
        .ok_or(MessageError::Wire(WireError::Overflow))
}

fn read_u32(bytes: &[u8], at: usize, endian: Endian) -> Result<u32, MessageError> {
    let end = at.checked_add(4).ok_or(MessageError::ShortHeader)?;
    let slice = bytes.get(at..end).ok_or(MessageError::ShortHeader)?;
    let word = <[u8; 4]>::try_from(slice).map_err(|_| MessageError::ShortHeader)?;
    Ok(match endian {
        Endian::Little => u32::from_le_bytes(word),
        Endian::Big => u32::from_be_bytes(word),
    })
}

/// The byte order the fixed header declares.
fn endian_of(bytes: &[u8]) -> Result<Endian, MessageError> {
    let byte = *bytes.first().ok_or(MessageError::ShortHeader)?;
    Endian::from_byte(byte).ok_or(MessageError::BadEndianness(byte))
}

/// The total length of the message whose fixed header begins `bytes`, or `None`
/// when fewer than `HEADER_LEN` bytes have arrived.
///
/// The transport needs this before it has a whole message, so the ceilings are
/// applied HERE as well as in `decode`: a length is what a reader would size a
/// buffer from, and refusing it late means having allocated for it first.
pub fn frame_len(bytes: &[u8]) -> Result<Option<usize>, MessageError> {
    if bytes.len() < HEADER_LEN {
        return Ok(None);
    }
    let endian = endian_of(bytes)?;
    match bytes.get(3).ok_or(MessageError::ShortHeader)? {
        &PROTOCOL_VERSION => {}
        other => return Err(MessageError::UnsupportedVersion(*other)),
    }
    let body_len = read_u32(bytes, 4, endian)?;
    let fields_len = read_u32(bytes, 12, endian)?;
    if body_len > MAX_BODY_BYTES {
        return Err(MessageError::BodyTooLarge);
    }
    if fields_len > MAX_HEADER_FIELDS_BYTES {
        return Err(MessageError::HeaderFieldsTooLarge);
    }
    let fields_end = HEADER_LEN
        .checked_add(usize::try_from(fields_len).map_err(|_| MessageError::HeaderFieldsTooLarge)?)
        .ok_or(MessageError::Wire(WireError::Overflow))?;
    let total = pad8(fields_end)?
        .checked_add(usize::try_from(body_len).map_err(|_| MessageError::BodyTooLarge)?)
        .ok_or(MessageError::Wire(WireError::Overflow))?;
    Ok(Some(total))
}

/// Decode one message from the front of `bytes`, returning it and the number of
/// bytes it occupied.
///
/// `received_fds` is how many descriptors actually arrived with it. It is a
/// parameter rather than a field because the count on the wire is a CLAIM: the
/// two are compared, and a disagreement disconnects the sender.
pub fn decode(bytes: &[u8], received_fds: u32) -> Result<(Message<'_>, usize), MessageError> {
    let total = frame_len(bytes)?.ok_or(MessageError::ShortHeader)?;
    if bytes.len() < total {
        return Err(MessageError::Truncated);
    }
    let endian = endian_of(bytes)?;
    let kind = MessageType::from_code(*bytes.get(1).ok_or(MessageError::ShortHeader)?)?;
    let flags = *bytes.get(2).ok_or(MessageError::ShortHeader)?;
    let body_len = usize::try_from(read_u32(bytes, 4, endian)?).map_err(|_| MessageError::BodyTooLarge)?;
    let serial = read_u32(bytes, 8, endian)?;
    if serial == 0 {
        return Err(MessageError::ZeroSerial);
    }

    // What ARRIVED bounds every `h` index, in a header field as much as in the
    // body: a limit of zero here would refuse a future field carrying one even
    // though the descriptors are in hand.
    if received_fds > MAX_FDS_PER_MESSAGE {
        return Err(MessageError::TooManyFds);
    }
    let limits = Limits { fds: received_fds };

    // Read the array from position 12 of the whole message so its 8-aligned
    // elements align against the message rather than against a slice.
    let mut reader = wire::Reader::at(bytes, 12, endian);
    let array = reader.value("a(yv)", limits)?;
    let entries = array
        .as_seq()
        .ok_or(MessageError::Wire(WireError::BadSignature))?;
    let fields = read_fields(&entries)?;
    let fields_end = reader.position();

    // The specification requires this padding to be zero, and every other
    // padding in the codec is checked — so without this up to seven bytes of
    // the sender's choosing ride inside a message that decodes, and two
    // distinct streams decode to the same message.
    let body_start = pad8(fields_end)?;
    let padding = bytes
        .get(fields_end..body_start)
        .ok_or(MessageError::Truncated)?;
    if padding.iter().any(|byte| *byte != 0) {
        return Err(MessageError::Wire(WireError::NonZeroPadding));
    }
    let body_end = body_start
        .checked_add(body_len)
        .ok_or(MessageError::Wire(WireError::Overflow))?;
    let body = bytes.get(body_start..body_end).ok_or(MessageError::Truncated)?;

    let declared_fds = fields.unix_fds.unwrap_or(0);
    if declared_fds > MAX_FDS_PER_MESSAGE {
        return Err(MessageError::TooManyFds);
    }
    if declared_fds != received_fds {
        return Err(MessageError::FdCountMismatch);
    }

    let signature = fields.signature.unwrap_or("");
    if body_len > 0 && fields.signature.is_none() {
        return Err(MessageError::BodyWithoutSignature);
    }
    // Before the body rather than after it: both depend only on the fields,
    // which are complete here, and walking 16 MiB to then refuse on a
    // malformed interface name is work a sender chooses for the broker.
    check_names(&fields)?;
    check_mandatory(kind, &fields)?;

    let args = wire::read_body(body, signature, endian, limits)?;

    Ok((
        Message {
            endian,
            kind,
            flags,
            serial,
            fields,
            args,
            body,
        },
        total,
    ))
}

impl<'a> Message<'a> {
    /// The body as it arrived, for a forwarder to copy. Its byte order is
    /// `self.endian`, which is the sender's and not necessarily the broker's.
    pub fn body_bytes(&self) -> &'a [u8] {
        self.body
    }
}

/// `decode` plus the checks that apply to a message arriving FROM A CLIENT.
///
/// The transport should reach for this rather than for `decode`: both of
/// `check_from_client`'s refusals are identity ones — a spoofed SENDER, a
/// forged local signal — and a caller that forgets the second call gets them
/// silently, with no compile error. `decode` stays public because the broker
/// decodes its OWN messages too, and those legitimately carry both.
pub fn decode_from_client(
    bytes: &[u8],
    received_fds: u32,
) -> Result<(Message<'_>, usize), MessageError> {
    let (message, consumed) = decode(bytes, received_fds)?;
    message.check_from_client()?;
    Ok((message, consumed))
}

fn read_fields<'a>(entries: &wire::Seq<'a>) -> Result<Fields<'a>, MessageError> {
    let mut fields = Fields::default();
    let mut seen = [false; 256];
    for entry in entries.values(wire::MAX_CONTAINER_ELEMENTS)? {
        let pair = entry
            .as_seq()
            .ok_or(MessageError::Wire(WireError::BadSignature))?
            .values(2)?;
        let code = match pair.first() {
            Some(Value::Byte(code)) => *code,
            _ => return Err(MessageError::Wire(WireError::BadSignature)),
        };
        let variant = pair
            .get(1)
            .and_then(Value::as_seq)
            .ok_or(MessageError::Wire(WireError::BadSignature))?;
        let inner = variant
            .values(1)?
            .first()
            .copied()
            .ok_or(MessageError::Wire(WireError::BadSignature))?;
        // Code 0 is INVALID rather than unknown, so it is refused rather than
        // ignored: it is the one code the specification says cannot appear.
        if code == 0 {
            return Err(MessageError::InvalidFieldCode);
        }
        let slot = seen
            .get_mut(usize::from(code))
            .ok_or(MessageError::InvalidFieldCode)?;
        if *slot {
            return Err(MessageError::DuplicateField(code));
        }
        *slot = true;
        // Unknown codes are ignored, but only AFTER their variant has been
        // parsed above: skipping one by length would trust bytes nothing read.
        let Some(field) = FieldCode::from_code(code) else {
            continue;
        };
        if variant.signature() != field.signature() {
            return Err(MessageError::FieldTypeMismatch(code));
        }
        match field {
            FieldCode::Path => fields.path = inner.as_str(),
            FieldCode::Interface => fields.interface = inner.as_str(),
            FieldCode::Member => fields.member = inner.as_str(),
            FieldCode::ErrorName => fields.error_name = inner.as_str(),
            FieldCode::Destination => fields.destination = inner.as_str(),
            FieldCode::Sender => fields.sender = inner.as_str(),
            FieldCode::Signature => fields.signature = inner.as_str(),
            FieldCode::ReplySerial => {
                // Zero is refused as a message serial, so a reply naming it
                // names a call no peer can ever have made.
                if inner.as_u32() == Some(0) {
                    return Err(MessageError::ZeroReplySerial);
                }
                fields.reply_serial = inner.as_u32();
            }
            FieldCode::UnixFds => fields.unix_fds = inner.as_u32(),
        }
    }
    Ok(fields)
}

/// The object path was validated by the `o` decode itself; these are the four
/// that arrive as a plain `s` and so have had no grammar applied to them.
fn check_names(fields: &Fields<'_>) -> Result<(), MessageError> {
    if let Some(interface) = fields.interface {
        if !name::valid_interface_name(interface) {
            return Err(MessageError::BadInterfaceName);
        }
    }
    if let Some(member) = fields.member {
        if !name::valid_member_name(member) {
            return Err(MessageError::BadMemberName);
        }
    }
    if let Some(error) = fields.error_name {
        if !name::valid_error_name(error) {
            return Err(MessageError::BadErrorName);
        }
    }
    for bus in [fields.destination, fields.sender].into_iter().flatten() {
        if !name::valid_bus_name(bus) {
            return Err(MessageError::BadBusName);
        }
    }
    Ok(())
}

fn check_mandatory(kind: MessageType, fields: &Fields<'_>) -> Result<(), MessageError> {
    let required: &[(&'static str, bool)] = match kind {
        MessageType::MethodCall => &[
            ("PATH", fields.path.is_some()),
            ("MEMBER", fields.member.is_some()),
        ],
        MessageType::MethodReturn => &[("REPLY_SERIAL", fields.reply_serial.is_some())],
        MessageType::Error => &[
            ("ERROR_NAME", fields.error_name.is_some()),
            ("REPLY_SERIAL", fields.reply_serial.is_some()),
        ],
        MessageType::Signal => &[
            ("PATH", fields.path.is_some()),
            ("INTERFACE", fields.interface.is_some()),
            ("MEMBER", fields.member.is_some()),
        ],
        // Nothing is mandatory on a type this version does not know, because
        // nothing here knows what it would mean.
        MessageType::Unknown(_) => &[],
    };
    for (what, present) in required {
        if !present {
            return Err(MessageError::MissingField(what));
        }
    }
    Ok(())
}

/// Compose a message.
///
/// Fields are emitted in ascending code order, so one set of arguments has one
/// encoding and a committed byte stream can be compared against it.
pub struct Builder<'a> {
    endian: Endian,
    kind: MessageType,
    flags: u8,
    serial: u32,
    fields: Fields<'a>,
    body_signature: &'a str,
    body: Vec<u8>,
}

impl<'a> Builder<'a> {
    /// The byte order is fixed at construction rather than passed to `encode`,
    /// so a body marshalled one way cannot be framed by a header declaring the
    /// other — which would decode as garbage rather than as an error.
    pub fn new(endian: Endian, kind: MessageType) -> Self {
        Self {
            endian,
            kind,
            flags: 0,
            serial: 0,
            fields: Fields::default(),
            body_signature: "",
            body: Vec::new(),
        }
    }

    /// The interface is an `Option` because the specification makes it optional
    /// on a call and mandatory on a signal — a distinction a second `&str`
    /// parameter would hide at every call site.
    pub fn method_call(
        endian: Endian,
        path: &'a str,
        interface: Option<&'a str>,
        member: &'a str,
    ) -> Self {
        let mut builder = Self::new(endian, MessageType::MethodCall);
        builder.fields.path = Some(path);
        builder.fields.interface = interface;
        builder.fields.member = Some(member);
        builder
    }

    pub fn method_return(endian: Endian, reply_serial: u32) -> Self {
        let mut builder = Self::new(endian, MessageType::MethodReturn);
        builder.fields.reply_serial = Some(reply_serial);
        builder.flags = FLAG_NO_REPLY_EXPECTED;
        builder
    }

    pub fn error(endian: Endian, error_name: &'a str, reply_serial: u32) -> Self {
        let mut builder = Self::new(endian, MessageType::Error);
        builder.fields.error_name = Some(error_name);
        builder.fields.reply_serial = Some(reply_serial);
        builder.flags = FLAG_NO_REPLY_EXPECTED;
        builder
    }

    pub fn signal(endian: Endian, path: &'a str, interface: &'a str, member: &'a str) -> Self {
        let mut builder = Self::new(endian, MessageType::Signal);
        builder.fields.path = Some(path);
        builder.fields.interface = Some(interface);
        builder.fields.member = Some(member);
        builder
    }

    pub fn serial(mut self, serial: u32) -> Self {
        self.serial = serial;
        self
    }

    pub fn flags(mut self, flags: u8) -> Self {
        self.flags = flags;
        self
    }

    pub fn destination(mut self, destination: &'a str) -> Self {
        self.fields.destination = Some(destination);
        self
    }

    pub fn sender(mut self, sender: &'a str) -> Self {
        self.fields.sender = Some(sender);
        self
    }

    pub fn unix_fds(mut self, count: u32) -> Self {
        self.fields.unix_fds = Some(count);
        self
    }

    /// Marshal the body. A fresh writer starts 8-aligned, which is where the
    /// body sits in the finished message.
    pub fn body<F>(mut self, signature: &'a str, fill: F) -> Result<Self, WireError>
    where
        F: FnOnce(&mut Writer) -> Result<(), WireError>,
    {
        wire::validate_signature(signature)?;
        let mut writer = Writer::new(self.endian);
        fill(&mut writer)?;
        self.body_signature = signature;
        self.body = writer.into_bytes();
        Ok(self)
    }

    /// Take an already-marshalled body verbatim.
    ///
    /// The caller owns the correctness of what it passes: these bytes are
    /// written into the message unread, so they must be a body of `signature`
    /// in THIS builder's byte order. It exists for forwarding, where both are
    /// true because the bytes came from a message this broker decoded — and
    /// where decoding-and-re-marshalling would be a second chance to change
    /// something a peer is entitled to have delivered unaltered.
    pub fn body_raw(mut self, signature: &'a str, body: Vec<u8>) -> Result<Self, WireError> {
        wire::validate_signature(signature)?;
        self.body_signature = signature;
        self.body = body;
        Ok(self)
    }

    pub fn encode(&self) -> Result<Vec<u8>, MessageError> {
        let endian = self.endian;
        if self.serial == 0 {
            return Err(MessageError::ZeroSerial);
        }
        let body_len =
            u32::try_from(self.body.len()).map_err(|_| MessageError::BodyTooLarge)?;
        if body_len > MAX_BODY_BYTES {
            return Err(MessageError::BodyTooLarge);
        }
        // `Unknown` is what `from_code` answers for a code this version does
        // not know, so one built by hand carrying a code it DOES know encodes a
        // message this crate's own `decode` reads as another type — or, for 0,
        // refuses outright. Emitting either is the asymmetry the body check
        // below exists to prevent, seen from the header.
        if MessageType::from_code(self.kind.code())? != self.kind {
            return Err(MessageError::InvalidType);
        }
        let mut fields = self.fields;
        if fields.reply_serial == Some(0) {
            return Err(MessageError::ZeroReplySerial);
        }
        if !self.body.is_empty() || !self.body_signature.is_empty() {
            fields.signature = Some(self.body_signature);
        }
        if fields.unix_fds.is_some_and(|n| n > MAX_FDS_PER_MESSAGE) {
            return Err(MessageError::TooManyFds);
        }
        check_names(&fields)?;
        check_mandatory(self.kind, &fields)?;
        // The writer does not check a closure against the signature it was
        // given, so this is where the two are compared: a body that disagrees
        // with its SIGNATURE is a disconnect at the peer, and the peer is the
        // worst place to find out.
        wire::read_body(
            &self.body,
            fields.signature.unwrap_or(""),
            endian,
            Limits {
                fds: fields.unix_fds.unwrap_or(0),
            },
        )?;

        let mut writer = Writer::new(endian);
        writer.byte(endian.byte());
        writer.byte(self.kind.code());
        writer.byte(self.flags);
        writer.byte(PROTOCOL_VERSION);
        writer.uint32(body_len);
        writer.uint32(self.serial);
        // The array's length lands at offset 12 and its contents at 16, which
        // is already 8-aligned — so this writes the last four bytes of the
        // fixed header as well as the fields.
        writer.array("(yv)", |w| write_fields(w, &fields))?;
        let fields_end = writer.len();
        let fields_len = fields_end
            .checked_sub(HEADER_LEN)
            .ok_or(MessageError::Wire(WireError::Overflow))?;
        if u32::try_from(fields_len).map_err(|_| MessageError::HeaderFieldsTooLarge)?
            > MAX_HEADER_FIELDS_BYTES
        {
            return Err(MessageError::HeaderFieldsTooLarge);
        }
        writer.align_to(8)?;
        writer.append(&self.body);
        Ok(writer.into_bytes())
    }
}

/// One header field's value, so the nine sit in ONE code-ordered table rather
/// than in two a loop has to interleave.
enum FieldValue<'a> {
    Text(&'a str),
    Number(u32),
}

fn write_fields(writer: &mut Writer, fields: &Fields<'_>) -> Result<(), WireError> {
    // The table IS the ascending code order, so one set of fields has exactly
    // one encoding and nothing sorts at run time.
    // `fields_are_written_in_ascending_code_order` is what checks the claim.
    let table: [(FieldCode, Option<FieldValue<'_>>); 9] = [
        (FieldCode::Path, fields.path.map(FieldValue::Text)),
        (FieldCode::Interface, fields.interface.map(FieldValue::Text)),
        (FieldCode::Member, fields.member.map(FieldValue::Text)),
        (FieldCode::ErrorName, fields.error_name.map(FieldValue::Text)),
        (
            FieldCode::ReplySerial,
            fields.reply_serial.map(FieldValue::Number),
        ),
        (FieldCode::Destination, fields.destination.map(FieldValue::Text)),
        (FieldCode::Sender, fields.sender.map(FieldValue::Text)),
        (FieldCode::Signature, fields.signature.map(FieldValue::Text)),
        (FieldCode::UnixFds, fields.unix_fds.map(FieldValue::Number)),
    ];
    for (field, value) in &table {
        let Some(value) = value else { continue };
        writer.structure(|w| {
            w.byte(field.code());
            w.variant(field.signature(), |w| match (field, value) {
                (FieldCode::Path, FieldValue::Text(text)) => w.object_path(text),
                (FieldCode::Signature, FieldValue::Text(text)) => w.signature(text),
                (_, FieldValue::Text(text)) => w.string(text),
                (_, FieldValue::Number(number)) => {
                    w.uint32(*number);
                    Ok(())
                }
            })
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Which fields are mandatory is per TYPE, and getting it wrong in either
    /// direction is invisible until a peer disconnects: too strict refuses
    /// ordinary traffic, too lax forwards a reply nothing can be matched to.
    #[test]
    fn each_type_names_its_own_mandatory_fields() {
        let endian = Endian::Little;
        for (kind, missing) in [
            (MessageType::MethodCall, "PATH"),
            (MessageType::MethodReturn, "REPLY_SERIAL"),
            (MessageType::Error, "ERROR_NAME"),
            (MessageType::Signal, "PATH"),
        ] {
            assert_eq!(
                Builder::new(endian, kind).serial(1).encode(),
                Err(MessageError::MissingField(missing)),
                "{kind:?} was encoded without {missing}"
            );
        }

        // A signal needs an interface where a call does not: a broadcast with
        // no interface cannot be matched by one.
        let mut call = Builder::new(endian, MessageType::MethodCall).serial(1);
        call.fields.path = Some("/a");
        call.fields.member = Some("M");
        assert!(call.encode().is_ok());
        let mut signal = Builder::new(endian, MessageType::Signal).serial(1);
        signal.fields.path = Some("/a");
        signal.fields.member = Some("M");
        assert_eq!(signal.encode(), Err(MessageError::MissingField("INTERFACE")));
    }

    /// An unknown type is ignored rather than refused, so it must decode; type
    /// 0 is INVALID and is the one that must not.
    #[test]
    fn an_unknown_type_decodes_and_type_zero_does_not() {
        let bytes = Builder::new(Endian::Little, MessageType::Unknown(9))
            .serial(1)
            .encode()
            .expect("an unknown type encodes");
        let (message, _) = decode(&bytes, 0).expect("an unknown type decodes");
        assert_eq!(message.kind, MessageType::Unknown(9));

        let mut invalid = bytes;
        if let Some(slot) = invalid.get_mut(1) {
            *slot = 0;
        }
        assert_eq!(decode(&invalid, 0), Err(MessageError::InvalidType));
    }

    #[test]
    fn a_serial_of_zero_is_not_encoded() {
        assert_eq!(
            Builder::method_call(Endian::Little, "/a", Some("a.b"), "M").encode(),
            Err(MessageError::ZeroSerial)
        );
    }

    /// The SIGNATURE field is present exactly when there is a body to describe.
    #[test]
    fn the_signature_field_tracks_the_body() {
        let empty = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .encode()
            .expect("an empty call encodes");
        let (message, _) = decode(&empty, 0).expect("decodes");
        assert_eq!(message.fields.signature, None);

        let full = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .body("s", |w| w.string("x"))
            .expect("a body marshals")
            .encode()
            .expect("a call with a body encodes");
        let (message, _) = decode(&full, 0).expect("decodes");
        assert_eq!(message.fields.signature, Some("s"));
        assert_eq!(message.args().len(), 1);
    }

    /// A body that does not match the SIGNATURE it is sent under is caught here
    /// rather than by the peer that would disconnect over it.
    #[test]
    fn a_body_that_disagrees_with_its_signature_is_refused_at_the_source() {
        let built = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .body("s", |w| {
                w.uint32(1);
                Ok(())
            })
            .expect("the writer does not relate the closure to the type")
            .encode();
        assert!(
            matches!(built, Err(MessageError::Wire(_))),
            "a u32 body was sent as a string: {built:?}"
        );
    }

    #[test]
    fn a_frame_length_needs_the_whole_fixed_header_first() {
        let bytes = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .encode()
            .expect("encodes");
        for short in 0..HEADER_LEN {
            assert_eq!(
                frame_len(bytes.get(..short).unwrap_or_default()),
                Ok(None),
                "{short} bytes should not name a frame length"
            );
        }
        assert_eq!(frame_len(&bytes), Ok(Some(bytes.len())));
        // A message with no body is a whole number of 8-byte units.
        assert_eq!(bytes.len() % 8, 0);
    }

    #[test]
    fn field_and_type_codes_round_trip() {
        for code in 1..=9u8 {
            let field = FieldCode::from_code(code).expect("a known field code");
            assert_eq!(field.code(), code);
            assert!(!field.signature().is_empty());
        }
        assert_eq!(FieldCode::from_code(0), None);
        assert_eq!(FieldCode::from_code(10), None);

        for kind in [
            MessageType::MethodCall,
            MessageType::MethodReturn,
            MessageType::Error,
            MessageType::Signal,
            MessageType::Unknown(200),
        ] {
            assert_eq!(MessageType::from_code(kind.code()), Ok(kind));
        }
        assert_eq!(MessageType::from_code(0), Err(MessageError::InvalidType));
    }

    /// The names that arrive as a plain `s` have had no grammar applied to them
    /// by the decode itself, so this is the only place they are checked.
    #[test]
    fn malformed_names_are_refused() {
        let endian = Endian::Little;
        assert_eq!(
            Builder::method_call(endian, "/a", Some("notdotted"), "M")
                .serial(1)
                .encode(),
            Err(MessageError::BadInterfaceName)
        );
        assert_eq!(
            Builder::method_call(endian, "/a", Some("a.b"), "has.dot")
                .serial(1)
                .encode(),
            Err(MessageError::BadMemberName)
        );
        assert_eq!(
            Builder::method_call(endian, "/a", Some("a.b"), "M")
                .destination("not a bus name")
                .serial(1)
                .encode(),
            Err(MessageError::BadBusName)
        );
        assert_eq!(
            Builder::error(endian, "notdotted", 1).serial(1).encode(),
            Err(MessageError::BadErrorName)
        );
        assert!(matches!(
            Builder::method_call(endian, "not/a/path", Some("a.b"), "M")
                .serial(1)
                .encode(),
            Err(MessageError::Wire(WireError::BadObjectPath))
        ));
    }

    /// The specification requires this padding to be zero, and it is the one
    /// padding in a message that `Reader::align` never sees: without the check,
    /// bytes of the sender's choosing ride inside a message that decodes, and
    /// two distinct streams decode to the same `Message`.
    #[test]
    fn the_padding_before_the_body_must_be_zero() {
        let bytes = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .body("y", |w| {
                w.byte(0x2a);
                Ok(())
            })
            .expect("a body marshals")
            .encode()
            .expect("encodes");
        let (message, _) = decode(&bytes, 0).expect("decodes");
        assert_eq!(message.args().len(), 1);

        // The padding is exactly what the declared field length leaves short of
        // the 8-aligned body, so it is derived rather than guessed at: the byte
        // below it is the last field's NUL, which is a zero and is not padding.
        let fields_end = HEADER_LEN
            + usize::try_from(read_u32(&bytes, 12, Endian::Little).expect("fields_len")).expect("fits");
        let body_start = pad8(fields_end).expect("body start");
        assert!(body_start > fields_end, "no pre-body padding to test");
        for at in fields_end..body_start {
            let mut dirty = bytes.clone();
            if let Some(slot) = dirty.get_mut(at) {
                *slot = 0xff;
            }
            assert_eq!(
                decode(&dirty, 0),
                Err(MessageError::Wire(WireError::NonZeroPadding)),
                "padding byte {at} was accepted dirty"
            );
        }
    }

    /// Code 0 is INVALID rather than merely unknown, so it is the one code that
    /// must be refused instead of ignored.
    #[test]
    fn header_field_code_zero_is_refused() {
        let bytes = Builder::method_call(Endian::Little, "/a", Some("a.b"), "M")
            .serial(1)
            .encode()
            .expect("encodes");
        // Offset 16 is the first field struct, whose first byte is its code.
        let mut invalid = bytes;
        if let Some(slot) = invalid.get_mut(16) {
            *slot = 0;
        }
        assert_eq!(decode(&invalid, 0), Err(MessageError::InvalidFieldCode));
    }

    /// Zero is refused as a message serial, so a reply naming it names a call
    /// no peer can ever have made.
    #[test]
    fn a_reply_serial_of_zero_is_refused_both_ways() {
        assert_eq!(
            Builder::method_return(Endian::Little, 0).serial(1).encode(),
            Err(MessageError::ZeroReplySerial)
        );
        let bytes = Builder::method_return(Endian::Little, 7)
            .serial(1)
            .encode()
            .expect("encodes");
        assert!(decode(&bytes, 0).is_ok());
        // REPLY_SERIAL is the only field, so its u32 is the last four bytes.
        let mut zeroed = bytes;
        let at = zeroed.len() - 4;
        for offset in at..zeroed.len() {
            if let Some(slot) = zeroed.get_mut(offset) {
                *slot = 0;
            }
        }
        assert_eq!(decode(&zeroed, 0), Err(MessageError::ZeroReplySerial));
    }

    /// A header field's variant is bounded by the descriptors that ARRIVED, not
    /// by zero: an unknown field carrying an `h` is one the specification says
    /// to ignore, and a limit of zero would disconnect over it instead.
    #[test]
    fn an_unknown_header_field_may_carry_a_descriptor_index() {
        let mut writer = Writer::new(Endian::Little);
        writer.byte(Endian::Little.byte());
        writer.byte(MessageType::MethodCall.code());
        writer.byte(0);
        writer.byte(PROTOCOL_VERSION);
        writer.uint32(0);
        writer.uint32(1);
        writer
            .array("(yv)", |w| {
                w.structure(|w| {
                    w.byte(FieldCode::Path.code());
                    w.variant("o", |w| w.object_path("/a"))
                })?;
                w.structure(|w| {
                    w.byte(FieldCode::Member.code());
                    w.variant("s", |w| w.string("M"))
                })?;
                w.structure(|w| {
                    w.byte(FieldCode::UnixFds.code());
                    w.variant("u", |w| {
                        w.uint32(1);
                        Ok(())
                    })
                })?;
                // 200 is not a code this version knows, so it is ignored — but
                // only after its variant has been read, which is the point.
                w.structure(|w| {
                    w.byte(200);
                    w.variant("h", |w| {
                        w.unix_fd(0);
                        Ok(())
                    })
                })
            })
            .expect("the field array marshals");
        writer.align_to(8).expect("the body aligns");
        let bytes = writer.into_bytes();
        let (message, _) = decode(&bytes, 1).expect("an h in an ignored field decodes");
        assert_eq!(message.fields.unix_fds, Some(1));
        // The index is still bounded: with nothing received, index 0 names a
        // descriptor that is not there and the field array itself refuses —
        // before the count comparison, which is why this is the error.
        assert_eq!(
            decode(&bytes, 0),
            Err(MessageError::Wire(WireError::FdIndexOutOfRange))
        );
    }

    /// `Unknown` is `from_code`'s answer for a code this version does not know,
    /// so one built by hand carrying a code it DOES know would encode a message
    /// this crate's own `decode` reads as another type — or refuses outright.
    #[test]
    fn a_builder_cannot_emit_a_type_its_own_decode_refuses() {
        for code in 0..=4u8 {
            assert_eq!(
                Builder::new(Endian::Little, MessageType::Unknown(code))
                    .serial(1)
                    .encode(),
                Err(MessageError::InvalidType),
                "Unknown({code}) was encoded"
            );
        }
        assert!(Builder::new(Endian::Little, MessageType::Unknown(5))
            .serial(1)
            .encode()
            .is_ok());
    }

    /// One set of fields must have exactly ONE encoding, which is what lets a
    /// committed byte stream be compared against a built one at all.
    #[test]
    fn fields_are_written_in_ascending_code_order() {
        // Every one of the nine, so reordering ANY row of the table reds this.
        // An `Error` carries 4..9 only, which left PATH, INTERFACE and MEMBER
        // unobserved by the test written to protect exactly their order.
        let mut builder = Builder::method_call(
            Endian::Little,
            "/org/example",
            Some("org.example.Iface"),
            "M",
        )
        .serial(1)
        .destination("org.example")
        .sender(":1.0")
        .unix_fds(0)
        .body("s", |w| w.string("x"))
        .expect("a body marshals");
        builder.fields.error_name = Some("org.example.Error.Failed");
        builder.fields.reply_serial = Some(7);
        let bytes = builder.encode().expect("encodes");
        let mut reader = wire::Reader::at(&bytes, 12, Endian::Little);
        let array = reader
            .value("a(yv)", Limits::NO_FDS)
            .expect("the fields decode");
        let codes: Vec<u8> = array
            .as_seq()
            .expect("an array")
            .values(16)
            .expect("entries")
            .iter()
            .filter_map(|entry| match entry.as_seq()?.values(2).ok()?.first()? {
                Value::Byte(code) => Some(*code),
                _ => None,
            })
            .collect();
        assert_eq!(codes, vec![1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    /// The bus's own local path and interface are not addresses a client may
    /// use. The refusal is `check_from_client`'s rather than `decode`'s,
    /// because the bus itself sends `Disconnected` from exactly these — so a
    /// codec that refused them outright could not marshal the one message they
    /// exist for.
    #[test]
    fn a_client_may_not_address_the_bus_s_own_local_names() {
        let disconnected = |path, interface| {
            Builder::signal(Endian::Little, path, interface, "Disconnected")
                .serial(1)
                .encode()
                .expect("the signal encodes")
        };
        for (path, interface) in [
            (LOCAL_PATH, LOCAL_INTERFACE),
            (LOCAL_PATH, "org.example.App"),
            ("/org/example", LOCAL_INTERFACE),
        ] {
            let bytes = disconnected(path, interface);
            let (message, _) = decode(&bytes, 0).expect("it is well-formed, so it decodes");
            assert_eq!(
                message.check_from_client(),
                Err(MessageError::ReservedLocalName),
                "{path} {interface} was accepted from a client"
            );
        }
        // Neither name alone is what is reserved about the other: an ordinary
        // signal with neither still passes.
        let ordinary = disconnected("/org/example", "org.example.App");
        let (message, _) = decode(&ordinary, 0).expect("decodes");
        assert_eq!(message.check_from_client(), Ok(()));
    }

    /// INTERFACE is optional on a method call and mandatory on a signal, and a
    /// call without one is ordinary traffic rather than an error.
    #[test]
    fn a_call_may_omit_its_interface() {
        let bytes = Builder::method_call(Endian::Little, "/a", None, "M")
            .serial(1)
            .encode()
            .expect("an interface-less call encodes");
        let (message, _) = decode(&bytes, 0).expect("decodes");
        assert_eq!(message.fields.interface, None);
        assert_eq!(message.fields.member, Some("M"));
    }
}
