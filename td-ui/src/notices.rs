//! The pinned face's provenance and licence texts, embedded from the
//! compositor's assets directory beside the face itself, for a program's
//! `--font-license` output. Data, not code: this is the one module outside
//! the pure set, and the only thing it does is carry these three strings.

pub const FONT_PROVENANCE: &str = include_str!("../../td-compositor/assets/PROVENANCE");
pub const FONT_COPYING: &str = include_str!("../../td-compositor/assets/unifont-COPYING");
pub const FONT_LICENSE: &str = include_str!("../../td-compositor/assets/unifont-OFL-1.1.txt");
