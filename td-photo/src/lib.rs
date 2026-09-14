#![forbid(unsafe_code)]

//! td's photo tool: the pure decoding and development core. `tiff` reads
//! the container, `nef` the Nikon layout and codec over it, `camera` is the
//! table of supported bodies, `color` the linear colour math and transfer,
//! `develop` the demosaic, resampler and pipeline, `jpeg` the baseline
//! decoder for the camera's embedded previews, `image` the RGB buffers
//! and PPM writer, `library` the sidecar grammar, roll rules and dating
//! rule, and `ui` the cull controller over td-ui's driven seam. None of
//! these reads a file, the environment, a clock or a descriptor: `main`
//! owns I/O. DESIGN.md is the contract.

pub mod camera;
pub mod color;
pub mod develop;
pub mod image;
pub mod jpeg;
pub mod library;
pub mod nef;
pub mod tiff;
pub mod ui;
