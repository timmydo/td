//! One 16550-compatible transmit channel at COM1.
use std::io::{self, Write};
#[derive(Default)]
pub(super) struct Uart {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scratch: u8,
    dll: u8,
    dlm: u8,
    thre: bool,
    fifo: bool,
}
impl Uart {
    pub(super) fn irq(&self) -> bool {
        self.thre && self.ier & 2 != 0 && self.mcr & 8 != 0
    }
    pub(super) fn read(&mut self, port: u16) -> u8 {
        match port - 0x3f8 {
            0 if self.lcr & 128 != 0 => self.dll,
            1 if self.lcr & 128 != 0 => self.dlm,
            1 => self.ier,
            2 => {
                let id = if self.thre && self.ier & 2 != 0 {
                    self.thre = false;
                    2
                } else {
                    1
                };
                id | if self.fifo { 0xc0 } else { 0 }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => 0x60,
            6 if self.mcr & 16 != 0 => {
                ((self.mcr & 2) << 3)
                    | ((self.mcr & 1) << 5)
                    | ((self.mcr & 4) << 4)
                    | ((self.mcr & 8) << 4)
            }
            6 => 0xb0,
            7 => self.scratch,
            _ => 0,
        }
    }
    pub(super) fn write(&mut self, port: u16, value: u8, out: &mut impl Write) -> io::Result<()> {
        match port - 0x3f8 {
            0 if self.lcr & 128 != 0 => self.dll = value,
            1 if self.lcr & 128 != 0 => self.dlm = value,
            0 => {
                out.write_all(&[value])?;
                self.thre = true;
            }
            1 => {
                if self.ier & 2 == 0 && value & 2 != 0 {
                    self.thre = true;
                }
                self.ier = value & 15;
            }
            2 => self.fifo = value & 1 != 0,
            3 => self.lcr = value,
            4 => self.mcr = value,
            7 => self.scratch = value,
            _ => {}
        }
        Ok(())
    }
}
