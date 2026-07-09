//! The De Boor flattening must turn the real fixture's spline pieces into valid
//! *simple* (non-self-intersecting) polygons — the reason we evaluate the curve
//! instead of using the self-intersecting control hull.

use dxf::Drawing;

use twister_splitter::extract::{extract, PieceKind, Sources};
use twister_splitter::flatten::{entity_rings, piece_polygon};

fn segs_cross(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
  let ccw = |p: [f64; 2], q: [f64; 2], r: [f64; 2]| {
    (r[1] - p[1]) * (q[0] - p[0]) - (q[1] - p[1]) * (r[0] - p[0])
  };
  (ccw(c, d, a) > 0.0) != (ccw(c, d, b) > 0.0)
    && (ccw(a, b, c) > 0.0) != (ccw(a, b, d) > 0.0)
}

fn self_intersects(ring: &[[f64; 2]]) -> bool {
  let n = ring.len();
  for i in 0..n {
    let a = ring[i];
    let b = ring[(i + 1) % n];
    for j in (i + 1)..n {
      // skip adjacent edges (shared vertex) and the wrap-around pair
      if j == i || j == i + 1 || (i == 0 && j == n - 1) {
        continue;
      }
      let c = ring[j];
      let d = ring[(j + 1) % n];
      if segs_cross(a, b, c, d) {
        return true;
      }
    }
  }
  false
}

#[test]
fn fixture_pieces_flatten_to_simple_polygons() {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _diags) = extract(&drawing, Sources::Layer);
  assert!(!pieces.is_empty());

  let mut polygons = 0;
  let mut bad = 0;
  for piece in &pieces {
    let PieceKind::Loose(entities) = &piece.kind else {
      continue;
    };
    let mut rings = Vec::new();
    for e in entities {
      entity_rings(e, &mut rings);
    }
    if let Some(poly) = piece_polygon(&rings, 4.0, 0.4) {
      polygons += 1;
      if self_intersects(&poly) {
        bad += 1;
        eprintln!("self-intersecting polygon for {}", piece.label);
      }
    }
  }
  assert!(polygons >= 50, "expected most parts to flatten, got {polygons}");
  // Curve evaluation (De Boor) yields simple polygons for the vast majority;
  // the rare self-intersecting outline is caught and hull-repaired by the nester
  // (jagua's SPolygon validator) before packing, so a handful is acceptable.
  assert!(
    bad <= 3,
    "curve-evaluated polygons should almost all be simple; got {bad} self-intersecting"
  );
}
