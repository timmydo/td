//! Bounded wl_pointer v5-v7 decoding and axis-frame accumulation.

use crate::layout::{CELL_HEIGHT, CELL_WIDTH};
use crate::wire::{Cursor, Message};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    Enter {
        serial: u32,
        surface: u32,
        x: i32,
        y: i32,
    },
    Leave(u32),
    Motion(i32, i32),
    Button {
        serial: u32,
        button: u32,
        pressed: bool,
    },
    Axis(usize, i32),
    Frame,
    Source(u32),
    Stop(usize),
    Discrete(usize, i32),
}

pub(crate) fn decode(message: &Message) -> Result<Event, String> {
    let mut c = Cursor::new(&message.payload);
    let event = match message.opcode {
        0 => Event::Enter {
            serial: c.u32()?,
            surface: c.u32()?,
            x: c.i32()?,
            y: c.i32()?,
        },
        1 => {
            c.u32()?;
            Event::Leave(c.u32()?)
        }
        2 => {
            c.u32()?;
            Event::Motion(c.i32()?, c.i32()?)
        }
        3 => {
            let serial = c.u32()?;
            c.u32()?;
            let button = c.u32()?;
            let pressed = match c.u32()? {
                0 => false,
                1 => true,
                _ => return Err("invalid pointer button state".into()),
            };
            Event::Button {
                serial,
                button,
                pressed,
            }
        }
        4 => {
            c.u32()?;
            Event::Axis(axis(c.u32()?)?, c.i32()?)
        }
        5 => Event::Frame,
        6 => {
            let source = c.u32()?;
            if source > 3 {
                return Err("invalid pointer axis source".into());
            }
            Event::Source(source)
        }
        7 => {
            c.u32()?;
            Event::Stop(axis(c.u32()?)?)
        }
        8 => Event::Discrete(axis(c.u32()?)?, c.i32()?),
        _ => return Err("unknown pointer event".into()),
    };
    c.finish()?;
    Ok(event)
}

fn axis(value: u32) -> Result<usize, String> {
    if value <= 1 {
        Ok(value as usize)
    } else {
        Err("invalid pointer axis".into())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Axis {
    distance: i64,
    steps: Option<i64>,
    remainder: i64,
    stop: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Wheel {
    axes: [Axis; 2],
    source: Option<u32>,
    events: usize,
}

impl Wheel {
    pub(crate) fn update(&mut self, event: Event) -> Result<(), String> {
        if self.events >= 256 {
            return Err("pointer axis frame budget".into());
        }
        self.events += 1;
        match event {
            Event::Source(source) => {
                if self.source != Some(source) {
                    for a in &mut self.axes {
                        a.remainder = 0;
                    }
                    self.source = Some(source);
                }
            }
            Event::Axis(index, value) | Event::Discrete(index, value) => {
                let a = self.axes.get_mut(index).ok_or("pointer axis index")?;
                if matches!(event, Event::Axis(..)) {
                    a.distance += i64::from(value);
                } else if value != 0 {
                    a.steps = Some(a.steps.unwrap_or(0) + i64::from(value));
                }
            }
            Event::Stop(index) => self.axes.get_mut(index).ok_or("pointer axis index")?.stop = true,
            _ => return Err("non-axis event in pointer wheel".into()),
        }
        Ok(())
    }

    pub(crate) fn frame(&mut self) -> (isize, isize) {
        let mut result = [0; 2];
        for ((axis, unit), output) in self
            .axes
            .iter_mut()
            // Native presentation currently has one surface unit per font
            // pixel (scale 1); keep smooth distances tied to cell metrics.
            .zip([CELL_HEIGHT as i64 * 256, CELL_WIDTH as i64 * 256])
            .zip(&mut result)
        {
            let delta = if let Some(steps) = axis.steps {
                axis.remainder = 0;
                steps * 3
            } else {
                let distance = axis.distance + axis.remainder;
                axis.remainder = distance % unit;
                distance / unit
            };
            *output = delta.clamp(-16_777_216, 16_777_216) as isize;
            axis.distance = 0;
            axis.steps = None;
            if axis.stop {
                axis.remainder = 0;
            }
            axis.stop = false;
        }
        self.events = 0;
        let [rows, columns] = result;
        (rows, columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(opcode: u16, words: &[u32]) -> Message {
        Message {
            object: 17,
            opcode,
            payload: words.iter().flat_map(|v| v.to_ne_bytes()).collect(),
        }
    }

    #[test]
    fn all_schemas_are_exact_and_unknown_values_are_refused() {
        for (opcode, words) in [
            (0, vec![1, 7, (-1i32) as u32, i32::MIN as u32]),
            (1, vec![1, 7]),
            (2, vec![0, 1, 2]),
            (3, vec![0, 0, 0x110, 1]),
            (4, vec![0, 1, 256]),
            (5, vec![]),
            (6, vec![3]),
            (7, vec![0, 0]),
            (8, vec![1, (-1i32) as u32]),
        ] {
            let m = message(opcode, &words);
            assert!(decode(&m).is_ok());
            for size in 0..m.payload.len() {
                let mut short = message(opcode, &words);
                short.payload.truncate(size);
                assert!(decode(&short).is_err());
            }
            let mut long = m;
            long.payload.push(0);
            assert!(decode(&long).is_err());
        }
        for m in [
            message(9, &[]),
            message(3, &[0, 0, 0x111, 2]),
            message(4, &[0, 2, 0]),
            message(6, &[4]),
            message(7, &[0, 2]),
            message(8, &[2, 0]),
        ] {
            assert!(decode(&m).is_err());
        }
        assert_eq!(
            decode(&message(0, &[1, 7, u32::MAX, i32::MIN as u32])).unwrap(),
            Event::Enter {
                serial: 1,
                surface: 7,
                x: -1,
                y: i32::MIN
            }
        );
    }

    #[test]
    fn discrete_and_diagonal_frames_never_double_count_distance() {
        let mut wheel = Wheel::default();
        wheel.update(Event::Discrete(0, 2)).unwrap();
        wheel.update(Event::Axis(0, 30000)).unwrap();
        wheel.update(Event::Source(0)).unwrap();
        wheel.update(Event::Discrete(1, -1)).unwrap();
        wheel.update(Event::Axis(1, -30000)).unwrap();
        assert_eq!(wheel.frame(), (6, -3));
        assert_eq!(wheel.frame(), (0, 0));
    }

    #[test]
    fn smooth_frames_carry_signed_fractions_and_stop_or_source_resets_them() {
        let mut wheel = Wheel::default();
        for _ in 0..3 {
            wheel.update(Event::Axis(0, 4 * 256)).unwrap();
            wheel.update(Event::Axis(1, -2 * 256)).unwrap();
            assert_eq!(wheel.frame(), (0, 0));
        }
        wheel.update(Event::Axis(0, 4 * 256)).unwrap();
        wheel.update(Event::Axis(1, -2 * 256)).unwrap();
        assert_eq!(wheel.frame(), (1, -1));
        wheel.update(Event::Axis(0, 15 * 256)).unwrap();
        wheel.update(Event::Stop(0)).unwrap();
        assert_eq!(wheel.frame(), (0, 0));
        wheel.update(Event::Axis(0, 256)).unwrap();
        assert_eq!(wheel.frame(), (0, 0));
        wheel.update(Event::Axis(0, 14 * 256)).unwrap();
        assert_eq!(wheel.frame(), (0, 0));
        wheel.update(Event::Source(1)).unwrap();
        wheel.update(Event::Axis(0, 256)).unwrap();
        assert_eq!(wheel.frame(), (0, 0));
    }

    #[test]
    fn frame_work_is_bounded_and_extreme_deltas_are_clamped_without_overflow() {
        let mut wheel = Wheel::default();
        for _ in 0..256 {
            wheel.update(Event::Axis(0, i32::MAX)).unwrap();
        }
        assert!(wheel.update(Event::Axis(0, 1)).is_err());
        assert_eq!(wheel.frame(), (16_777_216, 0));
        wheel.update(Event::Discrete(1, i32::MIN)).unwrap();
        assert_eq!(wheel.frame(), (0, -16_777_216));
    }

    #[test]
    fn zero_discrete_step_does_not_hide_smooth_motion_or_its_fraction() {
        let mut wheel = Wheel::default();
        wheel.update(Event::Axis(0, 8 * 256)).unwrap();
        assert_eq!(wheel.frame(), (0, 0));
        wheel.update(Event::Discrete(0, 0)).unwrap();
        wheel.update(Event::Axis(0, 8 * 256)).unwrap();
        assert_eq!(wheel.frame(), (1, 0));
    }
}
