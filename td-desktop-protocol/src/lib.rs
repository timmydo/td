//! One message per transport envelope. Images and text borrow the envelope.
//! Only ordinary application input is represented here.

use std::io::{self, Write};

pub const PROGRAM: &str = "td-desktop";
pub const VERSION: u32 = 1;
pub const MAX_TEXT: usize = 64 * 1024;
pub const MAX_PIXELS: usize = 4096 * 2160;
pub const MAX_REPLY: usize = 1 + 8 + 4 + MAX_PIXELS * 4;
pub const MAX_REQUEST: usize = 1 + MAX_TEXT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Invalid;

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid desktop message")
    }
}

impl std::error::Error for Invalid {}

/// Serial of the output geometry the viewer used to translate its pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output(pub std::num::NonZeroU64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Zero-based pixels from the top left of the named output.
pub struct Position {
    pub output: Output,
    pub x: u16,
    pub y: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    Frame,
    /// Linux evdev keycode, as sent by wl_keyboard.key, and its transition.
    Key {
        code: u16,
        down: bool,
    },
    /// Current buttons at the absolute position. Bits 0, 1 and 2 are left,
    /// right and middle respectively.
    Pointer {
        at: Position,
        buttons: u8,
    },
    /// Wheel travel in detents. Positive x scrolls right and positive y down.
    Scroll {
        at: Position,
        x: i16,
        y: i16,
    },
    ReleaseAll,
    ReadText,
    WriteText(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply<'a> {
    Applied,
    /// Ordinary desktop input or output is unavailable for this request.
    Unavailable,
    /// The viewer must fetch the new geometry before its next pointer event.
    StaleOutput,
    Text(&'a str),
    /// Tightly packed pixels, one blue, green, red and unused byte per pixel.
    Frame {
        output: Output,
        width: u16,
        height: u16,
        pixels: &'a [u8],
    },
}

fn take<const N: usize>(bytes: &mut &[u8]) -> Result<[u8; N], Invalid> {
    let (field, rest) = bytes.split_first_chunk::<N>().ok_or(Invalid)?;
    *bytes = rest;
    Ok(*field)
}

fn output(bytes: &mut &[u8]) -> Result<Output, Invalid> {
    std::num::NonZeroU64::new(u64::from_be_bytes(take(bytes)?))
        .map(Output)
        .ok_or(Invalid)
}

impl Position {
    fn read(bytes: &mut &[u8]) -> Result<Self, Invalid> {
        Ok(Self {
            output: output(bytes)?,
            x: u16::from_be_bytes(take(bytes)?),
            y: u16::from_be_bytes(take(bytes)?),
        })
    }

    fn write(self, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(&self.output.0.get().to_be_bytes())?;
        writer.write_all(&self.x.to_be_bytes())?;
        writer.write_all(&self.y.to_be_bytes())
    }
}

fn text(bytes: &[u8]) -> Result<&str, Invalid> {
    if bytes.len() > MAX_TEXT {
        return Err(Invalid);
    }
    std::str::from_utf8(bytes).map_err(|_| Invalid)
}

fn end(bytes: &[u8]) -> Result<(), Invalid> {
    if bytes.is_empty() {
        Ok(())
    } else {
        Err(Invalid)
    }
}

fn invalid_input() -> io::Error {
    io::ErrorKind::InvalidInput.into()
}

impl<'a> Request<'a> {
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self, Invalid> {
        let request = match take(&mut bytes)? {
            [1] => Self::Frame,
            [2] => {
                let code = u16::from_be_bytes(take(&mut bytes)?);
                let down = match take(&mut bytes)? {
                    [0] => false,
                    [1] => true,
                    _ => return Err(Invalid),
                };
                Self::Key { code, down }
            }
            [3] => {
                let at = Position::read(&mut bytes)?;
                let [buttons] = take(&mut bytes)?;
                if buttons > 7 {
                    return Err(Invalid);
                }
                Self::Pointer { at, buttons }
            }
            [4] => Self::Scroll {
                at: Position::read(&mut bytes)?,
                x: i16::from_be_bytes(take(&mut bytes)?),
                y: i16::from_be_bytes(take(&mut bytes)?),
            },
            [5] => Self::ReleaseAll,
            [6] => Self::ReadText,
            [7] => return Ok(Self::WriteText(text(bytes)?)),
            _ => return Err(Invalid),
        };
        end(bytes)?;
        Ok(request)
    }

    pub fn write(self, writer: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Frame => writer.write_all(&[1]),
            Self::Key { code, down } => {
                let [hi, lo] = code.to_be_bytes();
                writer.write_all(&[2, hi, lo, u8::from(down)])
            }
            Self::Pointer { at, buttons } if buttons <= 7 => {
                writer.write_all(&[3])?;
                at.write(writer)?;
                writer.write_all(&[buttons])
            }
            Self::Scroll { at, x, y } => {
                writer.write_all(&[4])?;
                at.write(writer)?;
                writer.write_all(&x.to_be_bytes())?;
                writer.write_all(&y.to_be_bytes())
            }
            Self::ReleaseAll => writer.write_all(&[5]),
            Self::ReadText => writer.write_all(&[6]),
            Self::WriteText(s) if s.len() <= MAX_TEXT => {
                writer.write_all(&[7])?;
                writer.write_all(s.as_bytes())
            }
            _ => Err(invalid_input()),
        }
    }
}

fn frame_size(width: u16, height: u16) -> Result<usize, Invalid> {
    let count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or(Invalid)?;
    if count == 0 || count > MAX_PIXELS {
        return Err(Invalid);
    }
    count.checked_mul(4).ok_or(Invalid)
}

impl<'a> Reply<'a> {
    pub fn decode(mut bytes: &'a [u8]) -> Result<Self, Invalid> {
        let reply = match take(&mut bytes)? {
            [1] => Self::Applied,
            [2] => Self::Unavailable,
            [3] => Self::StaleOutput,
            [4] => return Ok(Self::Text(text(bytes)?)),
            [5] => {
                let output = output(&mut bytes)?;
                let width = u16::from_be_bytes(take(&mut bytes)?);
                let height = u16::from_be_bytes(take(&mut bytes)?);
                if bytes.len() != frame_size(width, height)? {
                    return Err(Invalid);
                }
                return Ok(Self::Frame {
                    output,
                    width,
                    height,
                    pixels: bytes,
                });
            }
            _ => return Err(Invalid),
        };
        end(bytes)?;
        Ok(reply)
    }

    pub fn write(self, writer: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Applied => writer.write_all(&[1]),
            Self::Unavailable => writer.write_all(&[2]),
            Self::StaleOutput => writer.write_all(&[3]),
            Self::Text(s) if s.len() <= MAX_TEXT => {
                writer.write_all(&[4])?;
                writer.write_all(s.as_bytes())
            }
            Self::Frame {
                output,
                width,
                height,
                pixels,
            } if frame_size(width, height) == Ok(pixels.len()) => {
                writer.write_all(&[5])?;
                writer.write_all(&output.0.get().to_be_bytes())?;
                writer.write_all(&width.to_be_bytes())?;
                writer.write_all(&height.to_be_bytes())?;
                writer.write_all(pixels)
            }
            _ => Err(invalid_input()),
        }
    }
}
