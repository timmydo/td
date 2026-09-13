//! The supported bodies: one entry per exact `Make` and `Model`, with the
//! Adobe colour matrix from XYZ (D65) to camera space over 10000, the white
//! level and the default black level. An unknown body is refused by name
//! rather than developed with another body's colour (DESIGN.md, "Camera
//! table"). Adding one is a reviewed table entry with its source.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Camera {
    pub make: &'static str,
    pub model: &'static str,
    /// Default black level, used when the file's maker note carries none.
    pub black: u16,
    /// The sensor's white level in the file's sample units.
    pub white: u16,
    /// Rows map XYZ to camera R, G, B; integers over 10000.
    pub xyz_to_cam: [[i32; 3]; 3],
}

/// The table. The Z 8 entry is rawspeed's `cameras.xml` for mode
/// `14bit-compressed`.
pub const TABLE: &[Camera] = &[Camera {
    make: "NIKON CORPORATION",
    model: "NIKON Z 8",
    black: 1008,
    white: 15892,
    xyz_to_cam: [
        [11423, -4564, -1123],
        [-4816, 12895, 2119],
        [-210, 1061, 7282],
    ],
}];

/// The entry for a body, matched on the trimmed strings the file carries.
pub fn find(make: &str, model: &str) -> Option<&'static Camera> {
    let make = make.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    let model = model.trim_matches(|c: char| c == '\0' || c.is_whitespace());
    TABLE.iter().find(|c| c.make == make && c.model == model)
}
