//! The pinned face's provenance and licence texts, embedded from the
//! compositor's assets directory beside the face itself, for a program's
//! `--font-license` output, and where the outline face's own notices ship.
//! Data, not code: this is the one module outside the pure set, and the
//! only thing it does is carry these four strings.

pub const FONT_PROVENANCE: &str = include_str!("../../td-compositor/assets/PROVENANCE");
pub const FONT_COPYING: &str = include_str!("../../td-compositor/assets/unifont-COPYING");
pub const FONT_LICENSE: &str = include_str!("../../td-compositor/assets/unifont-OFL-1.1.txt");
pub const OUTLINE_FACE: &str = "The outline face is JetBrains Mono Nerd Font Mono from the Nerd Fonts v3.5.1 release, read from /etc/fonts/jetbrains-mono-nerd, where its licences ship beside it: OFL.txt, README.md (each merged icon set and its licence) and licenses/.\n";
