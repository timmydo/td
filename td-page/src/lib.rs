//! A styled page, made, laid out and painted (DESIGN.md): td-html's tree,
//! its sheets gathered and compiled by td-css, cascaded and laid out by
//! td-layout over td-ui's faces, and painted into a td-ui raster, with
//! the places, text, find and selection over a layout. No fetching, no
//! threads and no window: the caller brings the markup, the sheets and
//! the pictures, runs each step where it likes and shows the frame.
#![forbid(unsafe_code)]

pub mod charset;
pub mod fonts;
pub mod gather;
pub mod inline;
pub mod memory;
pub mod page;
pub mod paint;
pub mod pipeline;
pub mod search;
pub mod sheets;
pub mod text;
pub mod tree;
pub mod view;
