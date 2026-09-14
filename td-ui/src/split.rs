//! A bounded split with disjoint child geometry and a captured divider drag.
use crate::raster::{Draw, Primitive, Rect, Surface, BORDER, CHROME, SELECTED};

pub const DIVIDER: u32 = 8;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Horizontal,
    Vertical,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    pub axis: Axis,
    pub first_min: u32,
    pub second_min: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Share {
    first: u32,
    total: u32,
}
impl Share {
    pub fn new(first: u32, total: u32) -> Option<Self> {
        (total > 0 && first <= total).then_some(Self { first, total })
    }
    pub fn parts(self) -> (u32, u32) {
        (self.first, self.total)
    }
}
impl Default for Share {
    fn default() -> Self {
        Self { first: 1, total: 2 }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidSurface,
    InvalidRect,
    InvalidMinimum,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSurface => "invalid split surface",
            Self::InvalidRect => "split rectangle lies outside the surface",
            Self::InvalidMinimum => "invalid split child minimum",
        })
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub first: Rect,
    pub divider: Rect,
    pub second: Rect,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    Decrease,
    Increase,
    First,
    Last,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Event {
    Focus,
    Key(Key),
    Press { x: i64, y: i64 },
    Move { x: i64, y: i64 },
    Release { x: i64, y: i64 },
    FocusLost,
    Resize { surface: Surface, rect: Rect },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The new event is unclaimed; it may still retire an older capture.
    Ignored,
    Consumed,
    Changed,
}
#[derive(Clone, Copy, Debug)]
struct Drag {
    offset: i64,
    share: Share,
    extent: i64,
}
#[derive(Debug)]
pub struct Controller {
    config: Config,
    share: Share,
    surface: Surface,
    rect: Rect,
    layout: Option<Layout>,
    focused: bool,
    capture: Option<Drag>,
}
impl Controller {
    pub fn new(config: Config, share: Share, surface: Surface, rect: Rect) -> Result<Self, Error> {
        if config.first_min == 0
            || config.second_min == 0
            || config.first_min > crate::raster::MAX_AXIS as u32
            || config.second_min > crate::raster::MAX_AXIS as u32
        {
            return Err(Error::InvalidMinimum);
        }
        let mut split = Self {
            config,
            share,
            surface,
            rect,
            layout: None,
            focused: false,
            capture: None,
        };
        split.relayout(surface, rect)?;
        Ok(split)
    }
    fn relayout(&mut self, surface: Surface, rect: Rect) -> Result<(), Error> {
        surface.check().map_err(|_| Error::InvalidSurface)?;
        if rect.x < 0
            || rect.y < 0
            || i128::from(rect.x) + i128::from(rect.width) > surface.width as i128
            || i128::from(rect.y) + i128::from(rect.height) > surface.height as i128
        {
            return Err(Error::InvalidRect);
        }
        self.surface = surface;
        self.rect = rect;
        self.layout = self.calculate();
        Ok(())
    }
    fn extent(&self) -> u32 {
        match self.config.axis {
            Axis::Horizontal => self.rect.width,
            Axis::Vertical => self.rect.height,
        }
    }
    fn coordinate(&self, x: i64, y: i64) -> i64 {
        match self.config.axis {
            Axis::Horizontal => x,
            Axis::Vertical => y,
        }
    }
    fn start(&self) -> i64 {
        self.coordinate(self.rect.x, self.rect.y)
    }
    fn limits(&self) -> Option<(u32, u32, u32)> {
        let scale = self.surface.scale.value() as u32;
        let available = self.extent().checked_sub(DIVIDER * scale)?;
        let first = self.config.first_min * scale;
        let last = available.checked_sub(self.config.second_min * scale)?;
        (first <= last).then_some((available, first, last))
    }
    fn piece(&self, offset: u32, extent: u32) -> Rect {
        match self.config.axis {
            Axis::Horizontal => Rect {
                x: self.rect.x + i64::from(offset),
                width: extent,
                ..self.rect
            },
            Axis::Vertical => Rect {
                y: self.rect.y + i64::from(offset),
                height: extent,
                ..self.rect
            },
        }
    }
    fn calculate(&self) -> Option<Layout> {
        if self.rect.width == 0 || self.rect.height == 0 {
            return None;
        }
        let (available, min, max) = self.limits()?;
        let first = ((u64::from(available) * u64::from(self.share.first)
            / u64::from(self.share.total)) as u32)
            .clamp(min, max);
        let divider = DIVIDER * self.surface.scale.value() as u32;
        Some(Layout {
            first: self.piece(0, first),
            divider: self.piece(first, divider),
            second: self.piece(first + divider, available - first),
        })
    }
    pub fn layout(&self) -> Option<Layout> {
        self.layout
    }
    pub fn share(&self) -> Share {
        self.share
    }
    pub fn focused(&self) -> bool {
        self.focused
    }
    pub fn dragging(&self) -> bool {
        self.capture.is_some()
    }
    fn set_extent(&mut self, extent: i64) -> Outcome {
        let Some((total, min, max)) = self.limits() else {
            return Outcome::Ignored;
        };
        let first = extent.clamp(i64::from(min), i64::from(max)) as u32;
        // A stationary click must not replace an unclamped preference.
        if self.layout.is_some_and(|layout| {
            self.coordinate(layout.divider.x, layout.divider.y) - self.start() == i64::from(first)
        }) {
            return Outcome::Consumed;
        }
        self.share = Share { first, total };
        self.layout = self.calculate();
        Outcome::Changed
    }
    fn drag(&mut self, x: i64, y: i64) -> Outcome {
        let Some(capture) = self.capture else {
            return Outcome::Ignored;
        };
        self.set_extent(
            self.coordinate(x, y)
                .saturating_sub(self.start())
                .saturating_sub(capture.offset),
        )
    }
    pub fn event(&mut self, event: Event) -> Result<Outcome, Error> {
        Ok(match event {
            Event::Resize { surface, rect } => {
                self.capture = None;
                // Refused geometry must retire the old hit targets.
                self.layout = None;
                self.relayout(surface, rect)?;
                Outcome::Changed
            }
            Event::Focus => {
                let changed = !self.focused;
                self.focused = true;
                if changed {
                    Outcome::Changed
                } else {
                    Outcome::Consumed
                }
            }
            Event::FocusLost => {
                self.capture = None;
                let changed = self.focused;
                self.focused = false;
                if changed {
                    Outcome::Changed
                } else {
                    Outcome::Ignored
                }
            }
            Event::Press { x, y } => {
                self.capture = None;
                let Some(layout) = self.layout else {
                    return Ok(Outcome::Ignored);
                };
                if !layout.divider.contains(x, y) {
                    return Ok(Outcome::Ignored);
                }
                self.focused = true;
                let leading = self.coordinate(layout.divider.x, layout.divider.y);
                self.capture = Some(Drag {
                    offset: self.coordinate(x, y) - leading,
                    extent: leading - self.start(),
                    share: self.share,
                });
                Outcome::Changed
            }
            Event::Move { x, y } => self.drag(x, y),
            Event::Release { x, y } => {
                let result = self.drag(x, y);
                if let Some(capture) = self.capture.take() {
                    if self.share != capture.share
                        && self.layout.is_some_and(|layout| {
                            self.coordinate(layout.divider.x, layout.divider.y) - self.start()
                                == capture.extent
                        })
                    {
                        self.share = capture.share;
                        self.layout = self.calculate();
                        return Ok(Outcome::Changed);
                    }
                }
                result
            }
            Event::Key(key) => {
                self.capture = None;
                let Some(layout) = self.layout.filter(|_| self.focused) else {
                    return Ok(Outcome::Ignored);
                };
                let extent = self.coordinate(layout.divider.x, layout.divider.y) - self.start();
                let step = i64::from(DIVIDER) * self.surface.scale.value() as i64;
                self.set_extent(match key {
                    Key::Decrease => extent - step,
                    Key::Increase => extent + step,
                    Key::First => i64::MIN,
                    Key::Last => i64::MAX,
                })
            }
        })
    }
    pub fn emit(&self, damage: Rect, sink: &mut dyn FnMut(Draw)) {
        let Some(layout) = self.layout else {
            return;
        };
        let Some(clip) = layout.divider.intersection(damage) else {
            return;
        };
        sink(Draw {
            clip,
            primitive: Primitive::Fill {
                rect: layout.divider,
                color: if self.focused { SELECTED } else { BORDER },
            },
        });
        let scale = self.surface.scale.value() as u32;
        let mark = match self.config.axis {
            Axis::Horizontal => Rect {
                x: layout.divider.x + i64::from(3 * scale),
                y: layout.divider.y + i64::from(layout.divider.height / 4),
                width: 2 * scale,
                height: layout.divider.height / 2,
            },
            Axis::Vertical => Rect {
                x: layout.divider.x + i64::from(layout.divider.width / 4),
                y: layout.divider.y + i64::from(3 * scale),
                width: layout.divider.width / 2,
                height: 2 * scale,
            },
        };
        if let Some(clip) = mark.intersection(clip) {
            sink(Draw {
                clip,
                primitive: Primitive::Fill {
                    rect: mark,
                    color: CHROME,
                },
            });
        }
    }
}
