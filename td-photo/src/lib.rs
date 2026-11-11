#![forbid(unsafe_code)]

//! td's photo tool: the pure decoding and development core. `tiff` reads the
//! container, `nef` the Nikon layout and codec over it, `camera` is the table
//! of supported bodies, `color` the linear colour math and transfer, `develop`
//! the demosaic, resampler and pipeline, `jpeg` the thumbnail rule over
//! td-jpeg's preview decoder and the pool td-jpeg's encoder runs on for
//! export, `image` the PPM reader
//! and writer and the shrinker over td-jpeg's RGB buffer,
//! `library` the sidecar grammar, roll rules and dating rule, `look` the look
//! format with its built-in set, `auto` the auto exposure and contrast rule,
//! `settings` the export settings' grammar, `transform`, `cdf`, `deblock`,
//! `cdef` and `av1` the AV1 still picture encoder (the transforms and
//! quantizers, the default symbol probabilities, the deblocking and directional
//! filters shared through td-av1, and the coder), `avif` its container,
//! `exif` the Exif it carries,
//! and `ui` the cull controller
//! over td-ui's driven seam. None of these reads a file, the environment, a
//! clock or a descriptor: `main` owns I/O. DESIGN.md is the contract.

pub use td_av1::{cdef, cdf, deblock, transform};

pub mod auto;
pub mod av1;
pub mod avif;
pub mod camera;
pub mod color;
pub mod develop;
pub mod exif;
pub mod histogram;
pub mod image;
pub mod jpeg;
pub mod library;
pub mod look;
pub mod matching;
pub mod nef;
pub mod settings;
pub mod tiff;
pub mod ui;
