//! The rules a popup is placed by.
//!
//! An `xdg_positioner` is a set of rules rather than a position: a client fills
//! one in, hands it to `get_popup`, and the compositor derives a rectangle from
//! it. The protocol says the compositor COPIES those rules at that moment, so
//! the object can be reused or destroyed afterwards — which is why this type is
//! plain data with no identity of its own.
//!
//! Everything here is in the parent's window-geometry coordinates, which is
//! what `xdg_popup.configure` reports and what the anchor rectangle is measured
//! in.

/// A rectangle in the parent's window-geometry coordinates. Signed on every
/// side: an anchor rectangle may sit at a negative offset, and a popup placed
/// by gravity routinely lands left of or above the point it was anchored to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Which point of the anchor rectangle the popup is placed relative to. An
/// edge anchors to the middle of that edge and `None` to the middle of the
/// whole rectangle, which is why the two share their arms below.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    None,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

/// Which way the popup extends from that anchor point. The names are the
/// DIRECTION the surface is placed towards, so `Top` puts it ABOVE the point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Gravity {
    None,
    Top,
    Bottom,
    Left,
    Right,
    TopLeft,
    BottomLeft,
    TopRight,
    BottomRight,
}

/// Where along an axis a rule falls. Both enums above are the same nine
/// values twice over, and every use of either is one of these three answers
/// per axis — so the resolution asks the axis question rather than matching
/// nine arms twice and getting one of the eighteen wrong.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Edge {
    Low,
    Middle,
    High,
}

impl Anchor {
    /// The wire value, refused rather than defaulted: an anchor outside the
    /// enum names no point, and inventing one puts a menu somewhere arbitrary.
    pub fn from_wire(value: u32) -> Option<Anchor> {
        Some(match value {
            0 => Anchor::None,
            1 => Anchor::Top,
            2 => Anchor::Bottom,
            3 => Anchor::Left,
            4 => Anchor::Right,
            5 => Anchor::TopLeft,
            6 => Anchor::BottomLeft,
            7 => Anchor::TopRight,
            8 => Anchor::BottomRight,
            _ => return None,
        })
    }

    fn horizontal(self) -> Edge {
        match self {
            Anchor::Left | Anchor::TopLeft | Anchor::BottomLeft => Edge::Low,
            Anchor::Right | Anchor::TopRight | Anchor::BottomRight => Edge::High,
            Anchor::None | Anchor::Top | Anchor::Bottom => Edge::Middle,
        }
    }

    fn vertical(self) -> Edge {
        match self {
            Anchor::Top | Anchor::TopLeft | Anchor::TopRight => Edge::Low,
            Anchor::Bottom | Anchor::BottomLeft | Anchor::BottomRight => Edge::High,
            Anchor::None | Anchor::Left | Anchor::Right => Edge::Middle,
        }
    }
}

impl Gravity {
    pub fn from_wire(value: u32) -> Option<Gravity> {
        Some(match value {
            0 => Gravity::None,
            1 => Gravity::Top,
            2 => Gravity::Bottom,
            3 => Gravity::Left,
            4 => Gravity::Right,
            5 => Gravity::TopLeft,
            6 => Gravity::BottomLeft,
            7 => Gravity::TopRight,
            8 => Gravity::BottomRight,
            _ => return None,
        })
    }

    fn horizontal(self) -> Edge {
        match self {
            Gravity::Left | Gravity::TopLeft | Gravity::BottomLeft => Edge::Low,
            Gravity::Right | Gravity::TopRight | Gravity::BottomRight => Edge::High,
            Gravity::None | Gravity::Top | Gravity::Bottom => Edge::Middle,
        }
    }

    fn vertical(self) -> Edge {
        match self {
            Gravity::Top | Gravity::TopLeft | Gravity::TopRight => Edge::Low,
            Gravity::Bottom | Gravity::BottomLeft | Gravity::BottomRight => Edge::High,
            Gravity::None | Gravity::Left | Gravity::Right => Edge::Middle,
        }
    }
}

/// The rules as a client has filled them in so far. `size` and `anchor_rect`
/// are optional because a positioner is INCOMPLETE until both have been set,
/// and passing an incomplete one to `get_popup` is a protocol error — so the
/// absence has to survive as far as that check rather than being defaulted to
/// a rectangle nobody asked for.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Positioner {
    size: Option<(i32, i32)>,
    anchor_rect: Option<Rect>,
    anchor: Option<Anchor>,
    gravity: Option<Gravity>,
    constraint_adjustment: u32,
    offset: (i32, i32),
}

impl Positioner {
    /// A positive size, which the caller has already refused a client for
    /// getting wrong; this asserts nothing and records what it is given.
    pub fn set_size(&mut self, width: i32, height: i32) {
        self.size = Some((width, height));
    }

    pub fn set_anchor_rect(&mut self, rect: Rect) {
        self.anchor_rect = Some(rect);
    }

    pub fn set_anchor(&mut self, anchor: Anchor) {
        self.anchor = Some(anchor);
    }

    pub fn set_gravity(&mut self, gravity: Gravity) {
        self.gravity = Some(gravity);
    }

    /// A bitfield, and unknown bits are KEPT rather than refused: the protocol
    /// gives no error for one, and every bit in it is permission for the
    /// compositor to do something rather than a demand that it does.
    pub fn set_constraint_adjustment(&mut self, adjustment: u32) {
        self.constraint_adjustment = adjustment;
    }

    pub fn set_offset(&mut self, offset: (i32, i32)) {
        self.offset = offset;
    }

    /// Where the popup goes, in the parent's window-geometry coordinates, or
    /// `None` if the rules are incomplete — which is the caller's cue to raise
    /// `invalid_positioner` rather than to guess.
    ///
    /// The anchor point is derived from the anchor rectangle, the popup is hung
    /// off it by its gravity, and the client's own offset translates the
    /// result — the protocol's own worked example is that an anchor at (x, y)
    /// with a bottom-right gravity and an offset (ox, oy) gives (x + ox,
    /// y + oy). The three are summed, so where the offset appears in that sum
    /// is not observable; what it is FOR is aligning something inside the popup
    /// with something in the parent, and it is the offset position that
    /// constraint testing will use when that lands.
    pub fn resolve(&self) -> Option<Rect> {
        let (width, height) = self.size?;
        let anchor_rect = self.anchor_rect?;
        let anchor = self.anchor.unwrap_or(Anchor::None);
        let gravity = self.gravity.unwrap_or(Gravity::None);
        let x = place(
            anchor_rect.x,
            anchor_rect.width,
            anchor.horizontal(),
            width,
            gravity.horizontal(),
        )
        .saturating_add(self.offset.0);
        let y = place(
            anchor_rect.y,
            anchor_rect.height,
            anchor.vertical(),
            height,
            gravity.vertical(),
        )
        .saturating_add(self.offset.1);
        Some(Rect {
            x,
            y,
            width,
            height,
        })
    }
}

/// One axis of the placement: the anchor point along it, then the popup hung
/// off that point by its gravity. Halves round towards zero, as every
/// reference compositor's integer division does — a centred popup one pixel
/// narrower than its anchor rectangle has to land somewhere.
fn place(origin: i32, span: i32, anchor: Edge, size: i32, gravity: Edge) -> i32 {
    let point = origin.saturating_add(match anchor {
        Edge::Low => 0,
        Edge::Middle => span / 2,
        Edge::High => span,
    });
    point.saturating_add(match gravity {
        // Towards the low end, so the surface ENDS at the anchor point.
        Edge::Low => size.saturating_neg(),
        Edge::Middle => (size / 2).saturating_neg(),
        Edge::High => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANCHORS: [(u32, Anchor); 9] = [
        (0, Anchor::None),
        (1, Anchor::Top),
        (2, Anchor::Bottom),
        (3, Anchor::Left),
        (4, Anchor::Right),
        (5, Anchor::TopLeft),
        (6, Anchor::BottomLeft),
        (7, Anchor::TopRight),
        (8, Anchor::BottomRight),
    ];
    const GRAVITIES: [(u32, Gravity); 9] = [
        (0, Gravity::None),
        (1, Gravity::Top),
        (2, Gravity::Bottom),
        (3, Gravity::Left),
        (4, Gravity::Right),
        (5, Gravity::TopLeft),
        (6, Gravity::BottomLeft),
        (7, Gravity::TopRight),
        (8, Gravity::BottomRight),
    ];

    /// A 20x10 anchor rectangle at (100, 200), which no side of shares a value
    /// with another: an axis read off the wrong field is a different answer
    /// rather than the same one.
    fn rules() -> Positioner {
        let mut positioner = Positioner::default();
        positioner.set_size(8, 4);
        positioner.set_anchor_rect(Rect {
            x: 100,
            y: 200,
            width: 20,
            height: 10,
        });
        positioner
    }

    #[test]
    fn every_wire_value_of_both_enums_round_trips_and_nothing_else_is_taken() {
        for (value, anchor) in ANCHORS {
            assert_eq!(Anchor::from_wire(value), Some(anchor));
        }
        for (value, gravity) in GRAVITIES {
            assert_eq!(Gravity::from_wire(value), Some(gravity));
        }
        // The enums are closed at nine, so the first value past the end and
        // the far end of the range are both refused rather than defaulted.
        assert_eq!(Anchor::from_wire(9), None);
        assert_eq!(Gravity::from_wire(9), None);
        assert_eq!(Anchor::from_wire(u32::MAX), None);
        assert_eq!(Gravity::from_wire(u32::MAX), None);
    }

    /// The anchor POINT alone, isolated by a bottom-right gravity: that is the
    /// one arm that hangs the surface off the point by nothing, so the
    /// rectangle's own corner IS the point the rules name.
    #[test]
    fn each_anchor_names_the_edge_or_the_centre_it_is_called_after() {
        let expected = [
            (Anchor::None, (110, 205)),
            (Anchor::Top, (110, 200)),
            (Anchor::Bottom, (110, 210)),
            (Anchor::Left, (100, 205)),
            (Anchor::Right, (120, 205)),
            (Anchor::TopLeft, (100, 200)),
            (Anchor::BottomLeft, (100, 210)),
            (Anchor::TopRight, (120, 200)),
            (Anchor::BottomRight, (120, 210)),
        ];
        for (anchor, (x, y)) in expected {
            let mut positioner = rules();
            positioner.set_anchor(anchor);
            positioner.set_gravity(Gravity::BottomRight);
            assert_eq!(
                positioner.resolve(),
                Some(Rect {
                    x,
                    y,
                    width: 8,
                    height: 4
                }),
                "{anchor:?}"
            );
        }
    }

    /// Gravity hangs the surface off that point, and the NAME is the direction
    /// it is placed towards: `Top` puts it above, so the rectangle ENDS where
    /// the anchor point is.
    #[test]
    fn each_gravity_hangs_the_surface_the_way_it_is_named() {
        // Anchored top-left, so the point is (100, 200) and every answer below
        // is that point plus the gravity alone.
        let expected = [
            (Gravity::None, (96, 198)),
            (Gravity::Top, (96, 196)),
            (Gravity::Bottom, (96, 200)),
            (Gravity::Left, (92, 198)),
            (Gravity::Right, (100, 198)),
            (Gravity::TopLeft, (92, 196)),
            (Gravity::BottomLeft, (92, 200)),
            (Gravity::TopRight, (100, 196)),
            (Gravity::BottomRight, (100, 200)),
        ];
        for (gravity, (x, y)) in expected {
            let mut positioner = rules();
            positioner.set_anchor(Anchor::TopLeft);
            positioner.set_gravity(gravity);
            assert_eq!(
                positioner.resolve(),
                Some(Rect {
                    x,
                    y,
                    width: 8,
                    height: 4
                }),
                "{gravity:?}"
            );
        }
    }

    /// The ordinary menu: anchored to the bottom-left of a menu-bar item and
    /// dropping down-right from it, which is what every toolkit asks for and
    /// what a compositor that swapped anchor for gravity would get wrong in a
    /// way no single-axis test would show.
    #[test]
    fn a_menu_drops_below_the_item_it_was_anchored_to() {
        let mut positioner = rules();
        positioner.set_anchor(Anchor::BottomLeft);
        positioner.set_gravity(Gravity::BottomRight);
        assert_eq!(
            positioner.resolve(),
            Some(Rect {
                x: 100,
                y: 210,
                width: 8,
                height: 4
            })
        );
    }

    /// The offset translates the placed surface, and BOTH axes carry their own:
    /// the protocol's worked example is (x + ox, y + oy), so an offset dropped
    /// or applied to one axis alone is a menu beside where its client asked.
    #[test]
    fn the_offset_translates_the_placed_surface_on_both_axes() {
        let mut positioner = rules();
        positioner.set_anchor(Anchor::TopLeft);
        positioner.set_gravity(Gravity::TopLeft);
        positioner.set_offset((3, -5));
        assert_eq!(
            positioner.resolve(),
            Some(Rect {
                x: 95,
                y: 191,
                width: 8,
                height: 4
            })
        );
    }

    /// Incomplete rules resolve to nothing rather than to a default: the
    /// protocol makes an incomplete positioner an error at `get_popup`, and a
    /// rectangle invented here would place a menu the client never described.
    #[test]
    fn a_positioner_missing_either_half_resolves_to_nothing() {
        assert_eq!(Positioner::default().resolve(), None);

        let mut sized = Positioner::default();
        sized.set_size(8, 4);
        assert_eq!(sized.resolve(), None);

        let mut anchored = Positioner::default();
        anchored.set_anchor_rect(Rect {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        });
        assert_eq!(anchored.resolve(), None);

        // An anchor and a gravity are NOT required: the protocol defaults both
        // to `none`, so a client that sets only the two mandatory rules gets a
        // popup centred on its anchor rectangle rather than a refusal.
        let mut both = Positioner::default();
        both.set_size(8, 4);
        both.set_anchor_rect(Rect {
            x: 100,
            y: 200,
            width: 20,
            height: 10,
        });
        assert_eq!(
            both.resolve(),
            Some(Rect {
                x: 106,
                y: 203,
                width: 8,
                height: 4
            })
        );
    }

    /// A ZERO-area anchor rectangle is accepted and names a point. The protocol
    /// asks for a "non-zero anchor rectangle", but a client that gives an empty
    /// one has described somewhere perfectly well, and disconnecting it over a
    /// rule with a defined answer is the worse reading.
    #[test]
    fn an_empty_anchor_rectangle_is_a_point_rather_than_a_refusal() {
        let mut positioner = Positioner::default();
        positioner.set_size(8, 4);
        positioner.set_anchor_rect(Rect {
            x: 100,
            y: 200,
            width: 0,
            height: 0,
        });
        positioner.set_anchor(Anchor::BottomRight);
        positioner.set_gravity(Gravity::BottomRight);
        assert_eq!(
            positioner.resolve(),
            Some(Rect {
                x: 100,
                y: 200,
                width: 8,
                height: 4
            })
        );
    }

    /// Every arithmetic step saturates, because a client chooses all of it: an
    /// anchor rectangle at the end of the range, a size, and an offset, none of
    /// which the protocol bounds against the others.
    #[test]
    fn extreme_rules_saturate_rather_than_overflowing() {
        let mut positioner = Positioner::default();
        positioner.set_size(i32::MAX, i32::MAX);
        positioner.set_anchor_rect(Rect {
            x: i32::MAX,
            y: i32::MAX,
            width: i32::MAX,
            height: i32::MAX,
        });
        positioner.set_anchor(Anchor::BottomRight);
        positioner.set_gravity(Gravity::BottomRight);
        positioner.set_offset((i32::MAX, i32::MAX));
        assert_eq!(
            positioner.resolve(),
            Some(Rect {
                x: i32::MAX,
                y: i32::MAX,
                width: i32::MAX,
                height: i32::MAX
            })
        );

        let mut far = Positioner::default();
        far.set_size(i32::MAX, i32::MAX);
        far.set_anchor_rect(Rect {
            x: i32::MIN,
            y: i32::MIN,
            width: 0,
            height: 0,
        });
        far.set_anchor(Anchor::TopLeft);
        far.set_gravity(Gravity::TopLeft);
        far.set_offset((i32::MIN, i32::MIN));
        assert_eq!(
            far.resolve(),
            Some(Rect {
                x: i32::MIN,
                y: i32::MIN,
                width: i32::MAX,
                height: i32::MAX
            })
        );
    }

    /// The two axes are independent, which nine-by-nine over both enums is what
    /// proves: every pair places its x exactly as the x-only rules do and its y
    /// exactly as the y-only ones, so no arm of either match reads the other
    /// axis's field.
    #[test]
    fn the_axes_are_resolved_independently_of_each_other() {
        for (_, anchor) in ANCHORS {
            for (_, gravity) in GRAVITIES {
                let mut positioner = rules();
                positioner.set_anchor(anchor);
                positioner.set_gravity(gravity);
                let both = positioner.resolve().expect("complete rules resolve");

                let mut horizontal = rules();
                horizontal.set_anchor(anchor);
                horizontal.set_gravity(gravity);
                horizontal.set_anchor_rect(Rect {
                    x: 100,
                    y: 0,
                    width: 20,
                    height: 0,
                });
                let mut vertical = rules();
                vertical.set_anchor(anchor);
                vertical.set_gravity(gravity);
                vertical.set_anchor_rect(Rect {
                    x: 0,
                    y: 200,
                    width: 0,
                    height: 10,
                });
                assert_eq!(
                    both.x,
                    horizontal.resolve().expect("complete rules resolve").x,
                    "{anchor:?}/{gravity:?}"
                );
                assert_eq!(
                    both.y,
                    vertical.resolve().expect("complete rules resolve").y,
                    "{anchor:?}/{gravity:?}"
                );
            }
        }
    }
}
