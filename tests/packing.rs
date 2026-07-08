//! Full-precision invariants for the bin packer: placed pieces must stay within
//! the sheet and never overlap.

use twister_splitter::geom::Bbox;
use twister_splitter::pack::{pack, PackConfig, Placement};

fn bb(w: f64, h: f64) -> Bbox {
  // Offset the origin so the packer can't rely on boxes starting at (0,0).
  Bbox {
    min_x: 17.0,
    min_y: -9.0,
    max_x: 17.0 + w,
    max_y: -9.0 + h,
  }
}

/// Padded footprint (matching the packer's own math) of a placement.
fn footprint(p: &Placement, bbox: &Bbox, kerf: f64) -> (f64, f64, f64, f64) {
  let rotated = p.theta != 0.0;
  let w = if rotated { bbox.height() } else { bbox.width() } + kerf;
  let h = if rotated { bbox.width() } else { bbox.height() } + kerf;
  (p.x, p.y, w, h)
}

fn overlaps(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
  let e = 1e-9;
  a.0 < b.0 + b.2 - e && b.0 < a.0 + a.2 - e && a.1 < b.1 + b.3 - e && b.1 < a.1 + a.3 - e
}

fn assert_valid(bboxes: &[Bbox], cfg: &PackConfig) -> Vec<Placement> {
  let placements = pack(bboxes, cfg);
  assert_eq!(placements.len(), bboxes.len());

  // (1) Non-oversized pieces stay within the sheet.
  for p in &placements {
    if p.oversized {
      continue;
    }
    let (x, y, w, h) = footprint(p, &bboxes[p.piece_index], cfg.kerf);
    assert!(
      x >= -1e-9 && y >= -1e-9 && x + w <= cfg.sheet_w + 1e-9 && y + h <= cfg.sheet_h + 1e-9,
      "piece {} out of bounds: foot=({x},{y},{w},{h}) sheet={}x{}",
      p.piece_index,
      cfg.sheet_w,
      cfg.sheet_h,
    );
  }

  // (2) No two pieces on the same sheet overlap (full f64 precision).
  for i in 0..placements.len() {
    for j in (i + 1)..placements.len() {
      let (a, b) = (&placements[i], &placements[j]);
      if a.sheet != b.sheet || a.oversized || b.oversized {
        continue;
      }
      let fa = footprint(a, &bboxes[a.piece_index], cfg.kerf);
      let fb = footprint(b, &bboxes[b.piece_index], cfg.kerf);
      assert!(
        !overlaps(fa, fb),
        "pieces {} and {} overlap on sheet {}: {:?} vs {:?}",
        a.piece_index,
        b.piece_index,
        a.sheet,
        fa,
        fb,
      );
    }
  }

  placements
}

#[test]
fn many_varied_pieces_never_overlap() {
  // A deterministic spread of sizes that forces multiple sheets and rotations.
  let mut bboxes = Vec::new();
  for i in 0..80u32 {
    let w = 20.0 + ((i * 37) % 200) as f64;
    let h = 15.0 + ((i * 53) % 180) as f64;
    bboxes.push(bb(w, h));
  }
  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 400.0,
    kerf: 2.0,
    allow_rotation: true,
  };
  let placements = assert_valid(&bboxes, &cfg);
  let sheets = placements.iter().map(|p| p.sheet).max().unwrap() + 1;
  assert!(sheets >= 2, "varied pieces should need multiple sheets");
}

#[test]
fn piece_only_fits_when_rotated() {
  // 380 wide × 30 tall fits a 400×100 sheet either way; 90×380 only fits rotated.
  let bboxes = vec![bb(90.0, 380.0)];
  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 100.0,
    kerf: 0.0,
    allow_rotation: true,
  };
  let placements = assert_valid(&bboxes, &cfg);
  assert!(!placements[0].oversized);
  assert!(placements[0].theta != 0.0, "should have rotated to fit");
}

#[test]
fn oversized_piece_is_flagged_and_isolated() {
  let bboxes = vec![bb(50.0, 50.0), bb(500.0, 50.0), bb(50.0, 50.0)];
  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 400.0,
    kerf: 1.0,
    allow_rotation: true,
  };
  let placements = pack(&bboxes, &cfg);
  let big = &placements[1];
  assert!(big.oversized, "500mm piece must be flagged oversized");
  // Nothing else may share the oversized piece's sheet.
  for (i, p) in placements.iter().enumerate() {
    if i != 1 {
      assert_ne!(p.sheet, big.sheet, "piece {i} landed on the oversized sheet");
    }
  }
}

#[test]
fn no_rotation_when_disabled() {
  let bboxes = vec![bb(30.0, 60.0), bb(60.0, 30.0)];
  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 400.0,
    kerf: 0.0,
    allow_rotation: false,
  };
  let placements = pack(&bboxes, &cfg);
  for p in &placements {
    assert_eq!(p.theta, 0.0, "rotation was disabled");
  }
}
