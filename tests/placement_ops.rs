//! S-5 per-placement edit invariants: rotate is a faithful rigid transform
//! (four 90° turns return to start; a single turn preserves the footprint size
//! and rotation-about-centre keeps the piece local), and move_to_sheet reassigns
//! the sheet without disturbing the transform.

use twister_splitter::emit::Placed;
use twister_splitter::geom::{Affine, Bbox};

fn placed_at(theta: f64, tx: f64, ty: f64) -> Placed {
  let (s, c) = theta.sin_cos();
  Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine { m00: c, m01: -s, m10: s, m11: c, tx, ty },
    oversized: false,
    locked: false,
  }
}

fn footprint(p: &Placed, bb: &Bbox) -> (f64, f64, f64, f64) {
  let (mut nx, mut ny, mut xx, mut xy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
  for &(x, y) in &[(bb.min_x, bb.min_y), (bb.max_x, bb.min_y), (bb.max_x, bb.max_y), (bb.min_x, bb.max_y)] {
    let (wx, wy) = p.transform.apply(x, y);
    nx = nx.min(wx);
    ny = ny.min(wy);
    xx = xx.max(wx);
    xy = xy.max(wy);
  }
  (nx, ny, xx, xy)
}

#[test]
fn four_quarter_turns_return_to_start() {
  let bb = Bbox { min_x: 0.0, min_y: 0.0, max_x: 40.0, max_y: 20.0 };
  let mut p = placed_at(0.3, 100.0, 50.0);
  let before = p.transform;
  for _ in 0..4 {
    p.rotate(&bb, 1);
  }
  let a = p.transform;
  let d = |x: f64, y: f64| (x - y).abs();
  assert!(d(a.m00, before.m00) < 1e-9 && d(a.m01, before.m01) < 1e-9);
  assert!(d(a.m10, before.m10) < 1e-9 && d(a.m11, before.m11) < 1e-9);
  assert!(d(a.tx, before.tx) < 1e-9 && d(a.ty, before.ty) < 1e-9);
}

#[test]
fn quarter_turn_swaps_footprint_and_stays_centred() {
  let bb = Bbox { min_x: 0.0, min_y: 0.0, max_x: 40.0, max_y: 20.0 };
  let mut p = placed_at(0.0, 100.0, 50.0);
  let f0 = footprint(&p, &bb);
  let (cx0, cy0) = ((f0.0 + f0.2) * 0.5, (f0.1 + f0.3) * 0.5);

  p.rotate(&bb, 1);
  let f1 = footprint(&p, &bb);
  let (cx1, cy1) = ((f1.0 + f1.2) * 0.5, (f1.1 + f1.3) * 0.5);

  // A 90° turn swaps the footprint's width and height.
  assert!(((f1.2 - f1.0) - (f0.3 - f0.1)).abs() < 1e-9, "width should become old height");
  assert!(((f1.3 - f1.1) - (f0.2 - f0.0)).abs() < 1e-9, "height should become old width");
  // Rotation about the footprint centre keeps that centre fixed.
  assert!((cx1 - cx0).abs() < 1e-9 && (cy1 - cy0).abs() < 1e-9, "centre must not drift");
  // Still a rigid transform (rotation +90°, determinant +1).
  let t = p.transform;
  assert!((t.m00 * t.m11 - t.m01 * t.m10 - 1.0).abs() < 1e-9, "determinant must stay +1");
}

#[test]
fn move_to_sheet_only_changes_sheet() {
  let mut p = placed_at(0.5, 12.0, 34.0);
  let xf = p.transform;
  p.move_to_sheet(3);
  assert_eq!(p.sheet, 3);
  assert_eq!(p.transform.tx, xf.tx);
  assert_eq!(p.transform.ty, xf.ty);
}
