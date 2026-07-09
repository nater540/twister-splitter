//! End-to-end test over the real fixture: extract, pack, emit, reload, and
//! confirm nothing is lost, misplaced, or overlapping.

use std::path::PathBuf;

use dxf::Drawing;
use dxf::entities::EntityType;

use twister_splitter::emit::emit;
use twister_splitter::extract::{extract, Sources};
use twister_splitter::pack::{pack, PackConfig};

fn fixture() -> Drawing {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  Drawing::load_file(path).expect("load fixture")
}

#[test]
fn pipeline_preserves_geometry_and_avoids_overlap() {
  let drawing = fixture();
  let (pieces, _diags) = extract(&drawing, Sources::Both);

  // Per-part parts (multi-part layers split) + 6 block inserts.
  assert!(pieces.len() >= 60, "expected many parts, got {}", pieces.len());
  let n_inserts = pieces
    .iter()
    .filter(|p| p.label.starts_with("block:"))
    .count();
  assert_eq!(n_inserts, 6, "expected 6 block-reference pieces");

  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 400.0,
    kerf: 2.0,
    allow_rotation: true,
  };
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();
  let placements = pack(&bboxes, &cfg);

  // Block_0 is ~405mm tall — genuinely bigger than a 400mm bed, so the
  // axis-aligned rectangle packer flags it oversized.
  let oversized: Vec<&str> = placements
    .iter()
    .filter(|p| p.oversized)
    .map(|p| pieces[p.piece_index].label.as_str())
    .collect();
  assert!(oversized.contains(&"block:Block_0"), "Block_0 must be flagged oversized");

  // Full-precision overlap + bounds check on the real bounding boxes.
  for i in 0..placements.len() {
    let a = &placements[i];
    if !a.oversized {
      let bb = &bboxes[a.piece_index];
      let (w, h) = if a.theta != 0.0 {
        (bb.height(), bb.width())
      } else {
        (bb.width(), bb.height())
      };
      let (w, h) = (w + cfg.kerf, h + cfg.kerf);
      assert!(
        a.x >= -1e-9
          && a.y >= -1e-9
          && a.x + w <= cfg.sheet_w + 1e-9
          && a.y + h <= cfg.sheet_h + 1e-9,
        "piece {} out of bounds",
        pieces[a.piece_index].label
      );
    }
    for b in placements.iter().skip(i + 1) {
      if a.sheet != b.sheet || a.oversized || b.oversized {
        continue;
      }
      let fa = footprint(a, &bboxes[a.piece_index], cfg.kerf);
      let fb = footprint(b, &bboxes[b.piece_index], cfg.kerf);
      let e = 1e-9;
      let overlap = fa.0 < fb.0 + fb.2 - e
        && fb.0 < fa.0 + fa.2 - e
        && fa.1 < fb.1 + fb.3 - e
        && fb.1 < fa.1 + fa.3 - e;
      assert!(
        !overlap,
        "overlap on sheet {}: {} vs {}",
        a.sheet, pieces[a.piece_index].label, pieces[b.piece_index].label
      );
    }
  }

  // Emit and reload: all cut geometry survives, all fills are gone.
  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-out");
  let _ = std::fs::remove_dir_all(&out_dir);
  let placed: Vec<_> = placements
    .iter()
    .map(|p| p.to_placed(&bboxes[p.piece_index]))
    .collect();
  let report = emit(&drawing, &pieces, &placed, &out_dir, "gengar", 0.0).expect("emit");

  let mut splines = 0;
  let mut inserts = 0;
  let mut polylines = 0;
  let mut hatches = 0;
  for f in &report.files {
    let d = Drawing::load_file(f).expect("reload output");
    for e in d.entities() {
      match &e.specific {
        EntityType::Spline(_) => splines += 1,
        EntityType::Insert(_) => inserts += 1,
        EntityType::LwPolyline(_) => polylines += 1,
        // dxf crate can't represent hatch, but guard anyway.
        other => {
          if format!("{other:?}").starts_with("Hatch") {
            hatches += 1;
          }
        }
      }
    }
  }
  // Real spline outlines survive (per-part extraction drops zero-area artifacts).
  assert!(splines >= 100, "real spline outlines must survive, got {splines}");
  assert_eq!(inserts, 6, "all block references must survive");
  assert_eq!(polylines, 1, "the lone polyline must survive");
  assert_eq!(hatches, 0, "solid fills are intentionally dropped");
}

fn footprint(
  p: &twister_splitter::pack::Placement,
  bb: &twister_splitter::geom::Bbox,
  kerf: f64,
) -> (f64, f64, f64, f64) {
  let rotated = p.theta != 0.0;
  let w = if rotated { bb.height() } else { bb.width() } + kerf;
  let h = if rotated { bb.width() } else { bb.height() } + kerf;
  (p.x, p.y, w, h)
}
