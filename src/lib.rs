//! twister-splitter library: parse a DXF, split it into packable pieces, nest
//! them onto fixed-size sheets, and write one DXF per sheet.

pub mod emit;
pub mod extract;
pub mod flatten;
pub mod geom;
pub mod nest;
pub mod optimize;
pub mod pack;
