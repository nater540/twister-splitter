//! S-4 mirror/flip correctness. A reflected placement (determinant −1) must emit
//! geometrically correct DXF: arc sweeps reverse correctly, and a mirrored INSERT
//! carries `y_scale_factor = -1` so its block renders as the true reflection.
//! The `Placed::flip_*` helpers introduce such a reflection.

use std::path::PathBuf;

use dxf::entities::{Arc, Entity, EntityType, Insert, Line};
use dxf::{Block, Drawing, Point};

use twister_splitter::emit::{Placed, emit};
use twister_splitter::extract::{Piece, PieceKind, PieceSource};
use twister_splitter::geom::{Affine, Bbox};

fn bb(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Bbox {
  Bbox { min_x, min_y, max_x, max_y }
}

fn piece(kind: PieceKind) -> Piece {
  Piece { label: "t".into(), kind, bbox: bb(0.0, 0.0, 60.0, 60.0), area: 1.0, source: PieceSource::Part, id: 1, quantity: 1 }
}

/// Sample a DXF arc (CCW from `sd`° to `ed`°) into `n+1` points.
fn sample_arc(cx: f64, cy: f64, r: f64, sd: f64, ed: f64, n: usize) -> Vec<(f64, f64)> {
  let e = if ed < sd { ed + 360.0 } else { ed };
  (0..=n)
    .map(|i| {
      let t = (sd + (e - sd) * (i as f64 / n as f64)).to_radians();
      (cx + r * t.cos(), cy + r * t.sin())
    })
    .collect()
}

fn sorted(mut v: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
  v.sort_by(|a, b| a.partial_cmp(b).unwrap());
  v
}

fn emit_one(source: &Drawing, p: Piece, xf: Affine, stem: &str) -> Drawing {
  let placed = Placed { piece_index: 0, sheet: 0, transform: xf, oversized: false, locked: false };
  let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("target/test-mirror-{stem}"));
  let _ = std::fs::remove_dir_all(&dir);
  let report = emit(source, &[p], &[placed], &dir, stem, 0.0).expect("emit");
  Drawing::load_file(&report.files[0]).expect("reload")
}

#[test]
fn mirrored_arc_renders_as_the_reflected_arc() {
  let arc = Arc::new(Point::new(50.0, 20.0, 0.0), 10.0, 30.0, 100.0);
  let source = Drawing::new();
  let p = piece(PieceKind::Loose(vec![Entity::new(EntityType::Arc(arc.clone()))]));

  // Reflect across the x-axis: (x, y) -> (x, -y), determinant -1.
  let xf = Affine { m00: 1.0, m01: 0.0, m10: 0.0, m11: -1.0, tx: 0.0, ty: 0.0 };
  let d = emit_one(&source, p, xf, "arc");

  let got = d
    .entities()
    .find_map(|e| if let EntityType::Arc(a) = &e.specific { Some(a.clone()) } else { None })
    .expect("arc survived");

  // Expected: the reflected samples of the ORIGINAL arc.
  let expected: Vec<(f64, f64)> =
    sample_arc(50.0, 20.0, 10.0, 30.0, 100.0, 64).into_iter().map(|(x, y)| (x, -y)).collect();
  // Actual: samples of the EMITTED arc.
  let actual = sample_arc(got.center.x, got.center.y, got.radius, got.start_angle, got.end_angle, 64);

  let (e, a) = (sorted(expected), sorted(actual));
  assert_eq!(e.len(), a.len());
  for (p, q) in e.iter().zip(a.iter()) {
    assert!((p.0 - q.0).abs() < 1e-6 && (p.1 - q.1).abs() < 1e-6, "arc mismatch: {p:?} vs {q:?}");
  }
}

#[test]
fn mirrored_insert_places_block_points_at_the_reflected_position() {
  // Block with a diagonal line from base (0,0) to (5,3): (5,3) is a known vertex.
  let mut source = Drawing::new();
  let mut block = Block { name: "BM".into(), base_point: Point::new(0.0, 0.0, 0.0), ..Default::default() };
  block.entities.push(Entity::new(EntityType::Line(Line::new(Point::new(0.0, 0.0, 0.0), Point::new(5.0, 3.0, 0.0)))));
  source.add_block(block);

  let insert = Insert { name: "BM".into(), location: Point::new(0.0, 0.0, 0.0), ..Default::default() };
  let p = piece(PieceKind::Insert { insert: Box::new(Entity::new(EntityType::Insert(insert))), block_name: "BM".into() });

  // A genuine rotation+reflection (det -1): reflect across x, then rotate 40°.
  let xf = Affine::rotation_about(0.0, 0.0, 40f64.to_radians()).compose(&Affine::reflect_x(0.0));
  assert!(xf.determinant() < 0.0);
  let d = emit_one(&source, p, xf, "insert");

  let ins = d
    .entities()
    .find_map(|e| if let EntityType::Insert(i) = &e.specific { Some(i.clone()) } else { None })
    .expect("insert survived");
  assert_eq!(ins.y_scale_factor, -1.0, "mirror must set y_scale = -1");
  assert_eq!(ins.x_scale_factor, 1.0);

  // Render the block vertex (5,3) via the emitted INSERT and compare to xf(5,3).
  let (rot_s, rot_c) = ins.rotation.to_radians().sin_cos();
  let (bx, by) = (5.0 * ins.x_scale_factor, 3.0 * ins.y_scale_factor); // scale about base (0,0)
  let world = (
    ins.location.x + rot_c * bx - rot_s * by,
    ins.location.y + rot_s * bx + rot_c * by,
  );
  let want = xf.apply(5.0, 3.0);
  assert!(
    (world.0 - want.0).abs() < 1e-6 && (world.1 - want.1).abs() < 1e-6,
    "mirrored insert renders {world:?}, expected {want:?}"
  );
}

#[test]
fn flip_h_reflects_across_footprint_center() {
  let bbox = bb(0.0, 0.0, 40.0, 20.0);
  let mut p = Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine { m00: 1.0, m01: 0.0, m10: 0.0, m11: 1.0, tx: 100.0, ty: 50.0 },
    oversized: false,
    locked: false,
  };
  // Footprint spans x in [100,140]; centre x = 120.
  p.flip_h(&bbox);
  assert!(p.transform.determinant() < 0.0, "flip introduces a reflection");
  // The left edge (local x=0 -> world x=100) maps to the right edge (world 140).
  let (lx, _) = p.transform.apply(0.0, 0.0);
  assert!((lx - 140.0).abs() < 1e-9, "left edge should mirror to x=140, got {lx}");
  let (rx, _) = p.transform.apply(40.0, 0.0);
  assert!((rx - 100.0).abs() < 1e-9, "right edge should mirror to x=100, got {rx}");
}
