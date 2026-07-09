//! twister-splitter library: parse a DXF, split it into packable pieces, nest
//! them onto fixed-size sheets, and write one DXF per sheet.

pub mod diag;
pub mod emit;
pub mod extract;
pub mod flatten;
pub mod geom;
pub mod nest;
pub mod optimize;
pub mod pack;
pub mod stats;
pub mod svg;

// Desktop GUI (eframe/egui). Compiled only with `--features gui` so the CLI
// build never pulls in the windowing stack.
#[cfg(feature = "gui")]
pub mod gui;
