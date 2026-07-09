//! Per-sheet stats invariants over the real fixture: utilization is a sane
//! fraction, used area never exceeds the sheet, and every placed piece is
//! counted on exactly one sheet.

use dxf::Drawing;

use twister_splitter::extract::{Sources, extract};
use twister_splitter::nest;
use twister_splitter::optimize::nest_sheets;
use twister_splitter::stats::{CutProfile, all_sheet_stats};

const SHEET: f64 = 400.0;

#[test]
fn sheet_stats_are_sane() {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _diags) = extract(&drawing, Sources::Both);
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
    std::time::Duration::from_millis(600),
    std::time::Duration::from_millis(150),
    None,
    |_| {},
  );

  let stats = all_sheet_stats(&pieces, &drawing, &outcome.placed, outcome.sheets, SHEET, SHEET, CutProfile::default());
  assert_eq!(stats.len(), outcome.sheets);

  let mut counted = 0;
  for s in &stats {
    counted += s.piece_count;
    assert!(s.utilization >= 0.0, "utilization must be non-negative");
    // A non-oversized sheet can't be more than full; oversized sheets hold a
    // single part larger than the sheet, so allow those to exceed.
    let has_oversized = outcome
      .placed
      .iter()
      .any(|p| p.sheet == s.sheet && p.oversized);
    if !has_oversized {
      assert!(s.utilization <= 1.0, "sheet {} over-full: {}", s.sheet, s.utilization);
      assert!(s.used_area <= s.sheet_area + 1e-6);
    }
    // Cut length and pierces are populated when the sheet holds parts.
    if s.piece_count > 0 {
      assert!(s.cut_len_mm > 0.0, "sheet {} has parts but zero cut length", s.sheet);
      assert!(s.pierces > 0);
      assert!(s.est_run_secs > 0.0);
    }
  }
  assert_eq!(counted, outcome.placed.len(), "each placed piece counted once");
}
