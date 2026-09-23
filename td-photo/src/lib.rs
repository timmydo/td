#![forbid(unsafe_code)]

//! td's photo tool: the pure decoding and development core. `tiff` reads
//! the container, `nef` the Nikon layout and codec over it, `camera` is the
//! table of supported bodies, `color` the linear colour math and transfer,
//! `develop` the demosaic, resampler and pipeline, `jpeg` the baseline
//! decoder for the camera's embedded previews, `image` the RGB buffers
//! and PPM writer, `library` the sidecar grammar, roll rules and dating
//! rule, `look` the look format with its built-in set, `settings` the
//! export settings' grammar, `transform`, `cdf`, `deblock` and `av1` the
//! AV1 still picture encoder (the transforms and quantizers, the default
//! symbol probabilities, the deblocking filter, and the coder), `avif`
//! its container, and `ui` the cull controller over td-ui's driven seam.
//! None of these reads a file, the environment, a clock or a
//! descriptor: `main` owns I/O. DESIGN.md is the contract.

pub mod av1;
pub mod avif;
pub mod camera;
pub mod cdf;
pub mod color;
pub mod deblock;
pub mod develop;
pub mod image;
pub mod jpeg;
pub mod library;
pub mod look;
pub mod nef;
pub mod settings;
pub mod tiff;
pub mod transform;
pub mod ui;
