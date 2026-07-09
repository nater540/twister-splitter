//! P1-5 piece metadata invariants over the real fixture: area is populated,
//! sources are categorised, and ids are stable across re-extraction and unique.

use std::collections::HashSet;

use dxf::Drawing;

use twister_splitter::extract::{PieceSource, Sources, extract};

fn fixture() -> Drawing {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  Drawing::load_file(path).expect("load fixture")
}

#[test]
fn pieces_carry_area_source_and_stable_unique_ids() {
  let drawing = fixture();
  let (pieces, _d) = extract(&drawing, Sources::Both);

  // Every piece has a positive true area (degenerate parts are dropped upstream).
  for p in &pieces {
    assert!(p.area > 0.0, "piece {} has non-positive area {}", p.label, p.area);
  }

  // Sources are categorised: block:* -> Block, part:* -> Part.
  for p in &pieces {
    if p.label.starts_with("block:") {
      assert_eq!(p.source, PieceSource::Block);
    } else {
      assert_eq!(p.source, PieceSource::Part);
    }
  }
  assert!(pieces.iter().any(|p| p.source == PieceSource::Block), "fixture has block pieces");
  assert!(pieces.iter().any(|p| p.source == PieceSource::Part), "fixture has part pieces");

  // Ids are unique across the extraction (duplicate-identical parts still differ
  // via the occurrence counter).
  let ids: HashSet<u64> = pieces.iter().map(|p| p.id).collect();
  assert_eq!(ids.len(), pieces.len(), "piece ids must be unique");

  // Ids are stable across a second extraction of the same input+params.
  let (again, _d2) = extract(&drawing, Sources::Both);
  assert_eq!(again.len(), pieces.len());
  for (a, b) in pieces.iter().zip(again.iter()) {
    assert_eq!(a.id, b.id, "ids must be stable across re-extraction");
  }
}
