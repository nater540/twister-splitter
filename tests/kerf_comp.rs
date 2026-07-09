//! S-2 kerf compensation (Option A): the flattened outline is offset — outer
//! +kerf/2, holes −kerf/2 — and emitted as polylines; the nesting reservation
//! grows by the same kerf/2 so the enlarged cut still can't spill into a
//! neighbour (containment preserved).

use std::path::PathBuf;

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use twister_splitter::emit::{Placed, emit};
use twister_splitter::extract::{Piece, PieceKind, PieceSource, Sources, extract};
use twister_splitter::flatten;
use twister_splitter::geom::{Affine, Bbox};
use twister_splitter::nest;

fn wh(ring: &[[f64; 2]]) -> (f64, f64) {
  let (mut nx, mut ny, mut xx, mut xy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
  for &[x, y] in ring {
    nx = nx.min(x);
    ny = ny.min(y);
    xx = xx.max(x);
    xy = xy.max(y);
  }
  (xx - nx, xy - ny)
}

#[test]
fn outer_grows_and_holes_shrink_by_half_kerf() {
  // A 20×20 square with an 8×8 concentric hole.
  let outer = vec![[0.0, 0.0], [20.0, 0.0], [20.0, 20.0], [0.0, 20.0]];
  let hole = vec![[6.0, 6.0], [14.0, 6.0], [14.0, 14.0], [6.0, 14.0]];
  let comp = flatten::compensate_rings(&[outer, hole], 1.0); // half = 1 (kerf 2)

  let (ow, oh) = wh(&comp[0]);
  assert!((ow - 22.0).abs() < 1e-6 && (oh - 22.0).abs() < 1e-6, "outer must grow to 22×22, got {ow}×{oh}");
  let (hw, hh) = wh(&comp[1]);
  assert!((hw - 6.0).abs() < 1e-6 && (hh - 6.0).abs() < 1e-6, "hole must shrink to 6×6, got {hw}×{hh}");
}

fn area(ring: &[[f64; 2]]) -> f64 {
  let n = ring.len();
  let mut a = 0.0;
  for i in 0..n {
    let j = (i + 1) % n;
    a += ring[i][0] * ring[j][1] - ring[j][0] * ring[i][1];
  }
  a.abs() / 2.0
}

fn bbox(ring: &[[f64; 2]]) -> (f64, f64, f64, f64) {
  let (mut nx, mut ny, mut xx, mut xy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
  for &[x, y] in ring {
    nx = nx.min(x);
    ny = ny.min(y);
    xx = xx.max(x);
    xy = xy.max(y);
  }
  (nx, ny, xx, xy)
}

#[test]
fn compensated_outline_stays_within_its_reservation_on_fixture() {
  // On real (concave, spline) pieces, the kerf-compensated (enlarged) outline
  // must stay inside the nesting reservation, or a grown cut could spill into a
  // neighbour. Reservation is built from the compensated rings, so this holds by
  // construction; assert it as a regression guard (bbox containment).
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let (pieces, _d) = extract(&drawing, Sources::Both);

  let base = nest::build_items_with(&drawing, &pieces, 0.0);
  let comp = nest::build_items_with(&drawing, &pieces, 2.0); // kerf 2 -> grow by 1
  assert_eq!(base.len(), comp.len());

  let eps = 0.6; // RDP simplification slack, matching the pipeline's tolerance
  let mut any_grew = false;
  for (i, piece) in pieces.iter().enumerate() {
    // The compensated outline that will actually be emitted.
    let crings = flatten::compensate_rings(&piece.rings(&drawing), 1.0);
    let Some(outer) = crings.iter().filter(|r| r.len() >= 3).max_by(|a, b| {
      area(a).partial_cmp(&area(b)).unwrap()
    }) else { continue };
    let (onx, ony, oxx, oxy) = bbox(outer);
    let (rnx, rny, rxx, rxy) = bbox(&comp[i].polygon);
    assert!(
      onx >= rnx - eps && ony >= rny - eps && oxx <= rxx + eps && oxy <= rxy + eps,
      "piece {} compensated outline escapes its reservation: outline x[{onx:.1},{oxx:.1}] y[{ony:.1},{oxy:.1}] vs reservation x[{rnx:.1},{rxx:.1}] y[{rny:.1},{rxy:.1}]",
      piece.label
    );
    // Sanity that compensation actually enlarged most reservations.
    let (bw, bh) = wh(&base[i].polygon);
    let (cw, ch) = wh(&comp[i].polygon);
    if cw > bw + 1.0 && ch > bh + 1.0 {
      any_grew = true;
    }
  }
  assert!(any_grew, "kerf compensation should enlarge reservations");
}

#[test]
fn emit_kerf_comp_produces_grown_polyline_and_diagnostic() {
  // One square loose piece, emitted with kerf compensation on.
  let outer = [[0.0, 0.0], [20.0, 0.0], [20.0, 20.0], [0.0, 20.0]];
  let mut lw = LwPolyline::default();
  for &[x, y] in &outer {
    lw.vertices.push(LwPolylineVertex { x, y, ..Default::default() });
  }
  lw.set_is_closed(true);

  // LWPOLYLINE is an R2000+ entity; emit inherits the source version, so use a
  // modern one (an R12 source would drop it, exactly as it drops SPLINE).
  let mut source = Drawing::new();
  source.header.version = dxf::enums::AcadVersion::R2013;
  let piece = Piece {
    label: "sq".into(),
    kind: PieceKind::Loose(vec![Entity::new(EntityType::LwPolyline(lw))]),
    bbox: Bbox { min_x: 0.0, min_y: 0.0, max_x: 20.0, max_y: 20.0 },
    area: 400.0,
    source: PieceSource::Part,
    id: 1,
  };
  let placed = Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine { m00: 1.0, m01: 0.0, m10: 0.0, m11: 1.0, tx: 0.0, ty: 0.0 },
    oversized: false,
    locked: false,
  };
  let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-kerf");
  let _ = std::fs::remove_dir_all(&dir);
  let report = emit(&source, &[piece], &[placed], &dir, "sq", 2.0).expect("emit");
  assert!(!report.diagnostics.is_empty(), "kerf comp must return a diagnostic");

  let d = Drawing::load_file(&report.files[0]).expect("reload");
  let poly = d
    .entities()
    .find_map(|e| if let EntityType::LwPolyline(p) = &e.specific { Some(p.clone()) } else { None })
    .expect("compensated polyline emitted");
  let ring: Vec<[f64; 2]> = poly.vertices.iter().map(|v| [v.x, v.y]).collect();
  let (w, h) = wh(&ring);
  // Outer grew by half-kerf (1) on each side: 20 -> 22.
  assert!((w - 22.0).abs() < 1e-6 && (h - 22.0).abs() < 1e-6, "emitted outline should be 22×22, got {w}×{h}");
  assert!(poly.is_closed(), "compensated outline must be a closed polyline");
}
