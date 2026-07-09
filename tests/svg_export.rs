//! S-7 SVG export: sheets render to well-formed SVG (one file per sheet, or one
//! combined), each containing the expected number of `<svg>` roots and outline
//! polylines.

use std::path::PathBuf;

use dxf::Drawing;

use twister_splitter::extract::{Sources, extract};
use twister_splitter::nest;
use twister_splitter::optimize::nest_sheets;
use twister_splitter::svg;

const SHEET: f64 = 400.0;

fn nested() -> (Drawing, Vec<twister_splitter::extract::Piece>, Vec<twister_splitter::emit::Placed>, usize) {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _d) = extract(&drawing, Sources::Both);
  let items = nest::build_items(&drawing, &pieces);
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();
  let outcome = nest_sheets(
    &items,
    &bboxes,
    SHEET,
    SHEET,
    2.0,
    0.0,
    0x9E37_79B9_7F4A_7C15,
    std::time::Duration::from_millis(500),
    std::time::Duration::from_millis(120),
    None,
    |_| {},
  );
  (drawing, pieces, outcome.placed, outcome.sheets)
}

#[test]
fn per_sheet_and_combined_svg_are_well_formed() {
  let (drawing, pieces, placed, sheets) = nested();

  // In-memory: one SVG per sheet, each a single well-formed document with parts.
  let docs = svg::sheet_svgs(&drawing, &pieces, &placed, SHEET, SHEET);
  assert_eq!(docs.len(), sheets);
  for d in &docs {
    assert_eq!(d.matches("<svg").count(), 1, "each sheet is one <svg> root");
    assert!(d.contains("</svg>"));
    assert!(d.contains("<polyline"), "a sheet with parts should draw polylines");
  }

  // Combined: a single document holding every sheet's polylines.
  let combined = svg::combined_svg(&drawing, &pieces, &placed, SHEET, SHEET);
  assert_eq!(combined.matches("<svg").count(), 1);
  let total_polylines: usize = docs.iter().map(|d| d.matches("<polyline").count()).sum();
  assert_eq!(combined.matches("<polyline").count(), total_polylines, "combined keeps every outline");

  // Filesystem: writes the expected file set.
  let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-svg");
  let _ = std::fs::remove_dir_all(&out);
  let files = svg::write_svg(&drawing, &pieces, &placed, &out, "gengar", SHEET, SHEET, false).expect("write");
  assert_eq!(files.len(), sheets);
  for f in &files {
    assert!(f.exists() && std::fs::metadata(f).unwrap().len() > 0);
  }
  let one = svg::write_svg(&drawing, &pieces, &placed, &out, "gengar", SHEET, SHEET, true).expect("write combined");
  assert_eq!(one.len(), 1, "combined writes a single file");
}
