//! The parameter and result types td-mta stores, in PostgreSQL's binary
//! formats. Every parameter names its type to the server, and every
//! decode checks the column's type first: an `i64` reads `int8` and
//! nothing else, as the server's own binary input checks the reverse.
use crate::wire::{self, Reader};
use crate::Error;

/// The type OIDs this crate encodes and decodes (`pg_type.dat`).
pub mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const BOOL_ARRAY: u32 = 1000;
    pub const BYTEA_ARRAY: u32 = 1001;
    pub const TEXT_ARRAY: u32 = 1009;
    pub const INT8_ARRAY: u32 = 1016;
}

/// A parameter value. The server is told its type; `encode` appends its
/// binary form and returns false for SQL NULL, appending nothing.
pub trait Encode {
    fn type_oid(&self) -> u32;
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error>;
}

/// A type with one fixed OID, which a NULL of it or an array of it needs.
pub trait Typed {
    const OID: u32;
}

/// A type that may be an array element, and its array type's OID.
pub trait Element: Encode + Typed {
    const ARRAY_OID: u32;
}

macro_rules! fixed {
    ($type:ty, $oid:expr, $array:expr, |$value:ident| $bytes:expr) => {
        impl Typed for $type {
            const OID: u32 = $oid;
        }
        impl Encode for $type {
            fn type_oid(&self) -> u32 {
                $oid
            }
            fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
                let $value = *self;
                wire::put(out, &$bytes)?;
                Ok(true)
            }
        }
        impl Element for $type {
            const ARRAY_OID: u32 = $array;
        }
    };
}

fixed!(bool, oid::BOOL, oid::BOOL_ARRAY, |value| [u8::from(value)]);
fixed!(i64, oid::INT8, oid::INT8_ARRAY, |value| value.to_be_bytes());

impl Typed for i16 {
    const OID: u32 = oid::INT2;
}
impl Encode for i16 {
    fn type_oid(&self) -> u32 {
        oid::INT2
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        wire::put(out, &self.to_be_bytes())?;
        Ok(true)
    }
}

impl Typed for i32 {
    const OID: u32 = oid::INT4;
}
impl Encode for i32 {
    fn type_oid(&self) -> u32 {
        oid::INT4
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        wire::put(out, &self.to_be_bytes())?;
        Ok(true)
    }
}

impl Typed for [u8] {
    const OID: u32 = oid::BYTEA;
}
impl Encode for [u8] {
    fn type_oid(&self) -> u32 {
        oid::BYTEA
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        wire::put(out, self)?;
        Ok(true)
    }
}
impl Element for [u8] {
    const ARRAY_OID: u32 = oid::BYTEA_ARRAY;
}

impl Typed for Vec<u8> {
    const OID: u32 = oid::BYTEA;
}
impl Encode for Vec<u8> {
    fn type_oid(&self) -> u32 {
        oid::BYTEA
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        self.as_slice().encode(out)
    }
}
impl Element for Vec<u8> {
    const ARRAY_OID: u32 = oid::BYTEA_ARRAY;
}

/// Text is sent as its UTF-8 bytes, the connection's client encoding.
impl Typed for str {
    const OID: u32 = oid::TEXT;
}
impl Encode for str {
    fn type_oid(&self) -> u32 {
        oid::TEXT
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        // PostgreSQL text holds no NUL; refuse it here, before sending.
        if self.as_bytes().contains(&0) {
            return Err(Error::Invalid);
        }
        wire::put(out, self.as_bytes())?;
        Ok(true)
    }
}
impl Element for str {
    const ARRAY_OID: u32 = oid::TEXT_ARRAY;
}

impl Typed for String {
    const OID: u32 = oid::TEXT;
}
impl Encode for String {
    fn type_oid(&self) -> u32 {
        oid::TEXT
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        self.as_str().encode(out)
    }
}
impl Element for String {
    const ARRAY_OID: u32 = oid::TEXT_ARRAY;
}

impl<T: Typed + ?Sized> Typed for &T {
    const OID: u32 = T::OID;
}
impl<T: Encode + ?Sized> Encode for &T {
    fn type_oid(&self) -> u32 {
        (**self).type_oid()
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        (**self).encode(out)
    }
}
impl<T: Element + ?Sized> Element for &T {
    const ARRAY_OID: u32 = T::ARRAY_OID;
}

/// `None` is SQL NULL of `T`'s type.
impl<T: Typed> Typed for Option<T> {
    const OID: u32 = T::OID;
}
impl<T: Encode + Typed> Encode for Option<T> {
    fn type_oid(&self) -> u32 {
        T::OID
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        match self {
            Some(value) => value.encode(out),
            None => Ok(false),
        }
    }
}
impl<T: Element> Element for Option<T> {
    const ARRAY_OID: u32 = T::ARRAY_OID;
}

/// A one-dimensional array, lower bound 1; empty, it has no dimensions,
/// as PostgreSQL writes `'{}'`.
impl<T: Element> Encode for [T] {
    fn type_oid(&self) -> u32 {
        T::ARRAY_OID
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        let count = i32::try_from(self.len()).map_err(|_| Error::Invalid)?;
        wire::put_i32(out, i32::from(count != 0))?;
        let flags = out.len();
        wire::put_i32(out, 0)?;
        wire::put_u32(out, T::OID)?;
        if count != 0 {
            wire::put_i32(out, count)?;
            wire::put_i32(out, 1)?;
        }
        let mut nulls = false;
        for item in self {
            let at = out.len();
            wire::put_i32(out, 0)?;
            if item.encode(out)? {
                let length = out.len().saturating_sub(at + 4);
                wire::put_at(out, at, i32::try_from(length).map_err(|_| Error::Invalid)?)?;
            } else {
                wire::put_at(out, at, -1)?;
                nulls = true;
            }
        }
        if nulls {
            wire::put_at(out, flags, 1)?;
        }
        Ok(true)
    }
}

impl<T: Element> Encode for Vec<T> {
    fn type_oid(&self) -> u32 {
        T::ARRAY_OID
    }
    fn encode(&self, out: &mut Vec<u8>) -> Result<bool, Error> {
        self.as_slice().encode(out)
    }
}

/// Why a value could not be read as the type asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// No such column.
    Column,
    /// The column's type is not one this Rust type reads; its OID.
    Type(u32),
    /// The value is NULL and the Rust type is not an `Option`.
    Null,
    /// The bytes are not that type's binary form.
    Invalid,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Column => f.write_str("no such PostgreSQL column"),
            Self::Type(oid) => write!(f, "PostgreSQL column of type {oid}"),
            Self::Null => f.write_str("PostgreSQL value is NULL"),
            Self::Invalid => f.write_str("malformed PostgreSQL value"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A result value read from its column type and binary bytes, `None`
/// for NULL. Borrowed forms point into the result.
pub trait Decode<'a>: Sized {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError>;
}

fn present<'a>(
    type_oid: u32,
    want: &[u32],
    raw: Option<&'a [u8]>,
) -> Result<&'a [u8], DecodeError> {
    if !want.contains(&type_oid) {
        return Err(DecodeError::Type(type_oid));
    }
    raw.ok_or(DecodeError::Null)
}

impl<'a> Decode<'a> for bool {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
        match present(type_oid, &[oid::BOOL], raw)? {
            [0] => Ok(false),
            [1] => Ok(true),
            _ => Err(DecodeError::Invalid),
        }
    }
}

macro_rules! integer {
    ($type:ty, $oid:expr) => {
        impl<'a> Decode<'a> for $type {
            fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
                let bytes = present(type_oid, &[$oid], raw)?;
                bytes
                    .try_into()
                    .map(<$type>::from_be_bytes)
                    .map_err(|_| DecodeError::Invalid)
            }
        }
    };
}

integer!(i16, oid::INT2);
integer!(i32, oid::INT4);
integer!(i64, oid::INT8);

impl<'a> Decode<'a> for &'a [u8] {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
        present(type_oid, &[oid::BYTEA], raw)
    }
}

/// `text` and `name`, which arrive in the UTF-8 client encoding.
impl<'a> Decode<'a> for &'a str {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
        let bytes = present(type_oid, &[oid::TEXT, oid::NAME], raw)?;
        std::str::from_utf8(bytes).map_err(|_| DecodeError::Invalid)
    }
}

impl<'a, T: Decode<'a>> Decode<'a> for Option<T> {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
        match raw {
            None => Ok(None),
            Some(_) => T::decode(type_oid, raw).map(Some),
        }
    }
}

/// A one-dimensional array of `bool`, `bytea`, `text` or `int8`, read in
/// place. Its shape and every element's length are checked when it is
/// read; each element is decoded as it is iterated.
#[derive(Clone, Copy, Debug)]
pub struct Array<'a> {
    element: u32,
    count: usize,
    values: &'a [u8],
}

impl<'a> Array<'a> {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn element_oid(&self) -> u32 {
        self.element
    }

    /// Each element as `T`, in order.
    pub fn iter<T: Decode<'a>>(&self) -> impl Iterator<Item = Result<T, DecodeError>> + 'a {
        let element = self.element;
        let mut reader = Reader(self.values);
        (0..self.count).map(move |_| match wire::value(&mut reader) {
            Ok(raw) => T::decode(element, raw),
            Err(_) => Err(DecodeError::Invalid),
        })
    }
}

impl<'a> Decode<'a> for Array<'a> {
    fn decode(type_oid: u32, raw: Option<&'a [u8]>) -> Result<Self, DecodeError> {
        let element = match type_oid {
            oid::BOOL_ARRAY => oid::BOOL,
            oid::BYTEA_ARRAY => oid::BYTEA,
            oid::TEXT_ARRAY => oid::TEXT,
            oid::INT8_ARRAY => oid::INT8,
            other => return Err(DecodeError::Type(other)),
        };
        let mut reader = Reader(raw.ok_or(DecodeError::Null)?);
        let invalid = |_| DecodeError::Invalid;
        let dimensions = reader.i32().map_err(invalid)?;
        let flags = reader.i32().map_err(invalid)?;
        if reader.u32().map_err(invalid)? != element || !(flags == 0 || flags == 1) {
            return Err(DecodeError::Invalid);
        }
        let count = match dimensions {
            0 => 0,
            1 => {
                let count = usize::try_from(reader.i32().map_err(invalid)?)
                    .map_err(|_| DecodeError::Invalid)?;
                let _lower = reader.i32().map_err(invalid)?;
                count
            }
            _ => return Err(DecodeError::Invalid),
        };
        let values = reader.0;
        for _ in 0..count {
            if wire::value(&mut reader).map_err(invalid)?.is_none() && flags == 0 {
                return Err(DecodeError::Invalid);
            }
        }
        reader.end().map_err(invalid)?;
        Ok(Self {
            element,
            count,
            values,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn bytes(value: &dyn Encode) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        value.encode(&mut out).unwrap().then_some(out)
    }

    #[test]
    fn scalars_encode_in_binary_with_their_types() {
        assert_eq!(bytes(&true), Some(vec![1]));
        assert_eq!(bytes(&-2i16), Some(vec![0xff, 0xfe]));
        assert_eq!(bytes(&7i32), Some(vec![0, 0, 0, 7]));
        assert_eq!(bytes(&i64::MIN), Some(i64::MIN.to_be_bytes().to_vec()));
        assert_eq!(bytes(&&b"\0\xff"[..]), Some(vec![0, 0xff]));
        assert_eq!(bytes(&"é"), Some("é".as_bytes().to_vec()));
        assert_eq!(bytes(&None::<i64>), None);
        assert_eq!(None::<&str>.type_oid(), oid::TEXT);
        assert_eq!(Some(&b"x"[..]).type_oid(), oid::BYTEA);
        assert_eq!((&&5i64).type_oid(), oid::INT8);
        assert_eq!("a\0b".encode(&mut Vec::new()), Err(Error::Invalid));
    }

    #[test]
    fn arrays_encode_as_postgresql_writes_them() {
        assert_eq!(Vec::<i64>::new().type_oid(), oid::INT8_ARRAY);
        assert_eq!(
            bytes(&Vec::<i64>::new()),
            Some(vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 20])
        );
        let items: &[Option<&[u8]>] = &[Some(b"ab"), None];
        assert_eq!(items.type_oid(), oid::BYTEA_ARRAY);
        assert_eq!(
            bytes(&items),
            Some(vec![
                0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 17, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 2, b'a',
                b'b', 255, 255, 255, 255
            ])
        );
        let words = vec!["x", "yz"];
        let encoded = bytes(&words).unwrap();
        let array = Array::decode(oid::TEXT_ARRAY, Some(&encoded)).unwrap();
        let read: Vec<&str> = array.iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(read, ["x", "yz"]);
        assert_eq!(bytes(&&[true, false][..]).unwrap().len(), 20 + 2 * 5);
        // The owned forms td-mta builds encode as the borrowed ones do.
        let owned: Vec<Vec<u8>> = vec![b"ab".to_vec()];
        let borrowed: Vec<&[u8]> = vec![b"ab"];
        assert_eq!(bytes(&owned), bytes(&borrowed));
        let texts: Vec<String> = vec!["x".to_string()];
        assert_eq!(bytes(&texts), bytes(&vec!["x"]));
        let retries: Vec<Option<i64>> = vec![Some(1), None];
        assert_eq!(retries.type_oid(), oid::INT8_ARRAY);
        assert_eq!(bytes(&retries).unwrap()[7], 1, "has NULLs");
    }

    #[test]
    fn decoding_checks_type_length_and_null() {
        assert_eq!(
            i64::decode(oid::INT4, Some(&[0; 8])),
            Err(DecodeError::Type(23))
        );
        assert_eq!(
            i64::decode(oid::INT8, Some(&[0; 4])),
            Err(DecodeError::Invalid)
        );
        assert_eq!(i64::decode(oid::INT8, None), Err(DecodeError::Null));
        assert_eq!(Option::<i64>::decode(oid::INT8, None), Ok(None));
        assert_eq!(
            Option::<i64>::decode(oid::TEXT, Some(b"1")),
            Err(DecodeError::Type(25))
        );
        assert_eq!(i16::decode(oid::INT2, Some(&[1, 2])), Ok(0x0102));
        assert_eq!(i32::decode(oid::INT4, Some(&[0, 0, 1, 0])), Ok(256));
        assert_eq!(
            bool::decode(oid::BOOL, Some(&[2])),
            Err(DecodeError::Invalid)
        );
        assert_eq!(
            bool::decode(oid::BOOL, Some(&[])),
            Err(DecodeError::Invalid)
        );
        assert_eq!(<&str>::decode(oid::NAME, Some(b"td")), Ok("td"));
        assert_eq!(
            <&str>::decode(oid::TEXT, Some(b"\xff")),
            Err(DecodeError::Invalid)
        );
        assert_eq!(
            <&str>::decode(oid::BYTEA, Some(b"x")),
            Err(DecodeError::Type(17))
        );
        assert_eq!(
            <&[u8]>::decode(oid::TEXT, Some(b"x")),
            Err(DecodeError::Type(25))
        );
    }

    #[test]
    fn hostile_arrays_are_refused() {
        let header = |dims: i32, flags: i32, element: u32| {
            let mut out = Vec::new();
            out.extend_from_slice(&dims.to_be_bytes());
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&element.to_be_bytes());
            out
        };
        let mut two_dims = header(2, 0, 20);
        two_dims.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1]);
        let mut negative = header(1, 0, 20);
        negative.extend_from_slice(&[255, 255, 255, 255, 0, 0, 0, 1]);
        let mut short = header(1, 0, 20);
        short.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 1]);
        let mut hidden_null = header(1, 0, 17);
        hidden_null.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 255, 255, 255, 255]);
        let mut trailing = header(0, 0, 20);
        trailing.push(0);
        for (oid, bad) in [
            (oid::INT8_ARRAY, two_dims),
            (oid::INT8_ARRAY, negative),
            (oid::INT8_ARRAY, short),
            (oid::BYTEA_ARRAY, hidden_null),
            (oid::INT8_ARRAY, trailing),
            (oid::INT8_ARRAY, header(0, 2, 20)),
            (oid::INT8_ARRAY, header(0, 0, 25)),
            (oid::INT8_ARRAY, vec![0, 0, 0]),
        ] {
            assert_eq!(
                Array::decode(oid, Some(&bad)).err(),
                Some(DecodeError::Invalid)
            );
        }
        assert_eq!(
            Array::decode(oid::INT8, Some(&header(0, 0, 20))).err(),
            Some(DecodeError::Type(20))
        );
        // A well-formed element of the wrong length reds when it is read.
        let mut wrong = header(1, 0, 20);
        wrong.extend_from_slice(&[0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 2, 0, 1]);
        let array = Array::decode(oid::INT8_ARRAY, Some(&wrong)).unwrap();
        assert_eq!(array.iter::<i64>().next(), Some(Err(DecodeError::Invalid)));
    }
}
