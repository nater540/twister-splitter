//! R3-2 (engraved assembly numbers) and R3-4 (holes-before-outline cut order).
//!
//! A piece made of an outer square plus a smaller inner square (a hole) must
//! emit the hole *before* the outer contour, and — with engraving on — carry a
//! centred TEXT label of its assembly number on the `Engrave` layer.

use std::path::PathBuf;

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use twister_splitter::emit::{emit_opts, EmitOptions};
use twister_splitter::extract::{Piece, PieceKind, PieceSource};
use twister_splitter::geom::{Affine, Bbox};

/// A closed square LwPolyline entity of side `s` offset to `(ox, oy)`.
fn square(ox: f64, oy: f64, s: f64) -> Entity {
  let mut lw = LwPolyline::default();
  for &(x, y) in &[(0.0, 0.0), (s, 0.0), (s, s), (0.0, s)] {
    lw.vertices.push(LwPolylineVertex { x: ox + x, y: oy + y, ..Default::default() });
  }
  lw.set_is_closed(true);
  Entity::new(EntityType::LwPolyline(lw))
}

/// Absolute shoelace area of an LwPolyline entity.
fn poly_area(e: &Entity) -> f64 {
  let EntityType::LwPolyline(p) = &e.specific else { return 0.0 };
  let v = &p.vertices;
  let mut a = 0.0;
  for i in 0..v.len() {
    let j = (i + 1) % v.len();
    a += v[i].x * v[j].y - v[j].x * v[i].y;
  }
  a.abs() / 2.0
}

#[test]
fn engraves_number_and_cuts_holes_before_outline() {
  let mut drawing = Drawing::new();
  drawing.header.version = dxf::enums::AcadVersion::R2013;

  // Outer 20×20 square with an inner 6×6 hole — two loose entities in one piece.
  let outer = square(0.0, 0.0, 20.0);
  let hole = square(7.0, 7.0, 6.0);
  let piece = Piece {
    label: "framed".into(),
    kind: PieceKind::Loose(vec![outer, hole]),
    bbox: Bbox { min_x: 0.0, min_y: 0.0, max_x: 20.0, max_y: 20.0 },
    area: 400.0 - 36.0,
    source: PieceSource::Part,
    id: 1,
    quantity: 1,
  };
  let placed = twister_splitter::emit::Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine::place(&piece.bbox, 0.0, 0.0, 0.0),
    oversized: false,
    locked: false,
  };

  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-engrave");
  let _ = std::fs::remove_dir_all(&out_dir);
  let opts = EmitOptions { engrave_numbers: true, ..Default::default() };
  let report = emit_opts(&drawing, &[piece], &[placed], &out_dir, "framed", opts).expect("emit");

  let d = Drawing::load_file(&report.files[0]).expect("reload");

  // R3-4: the two cut polylines are emitted hole-first (smaller area first).
  let polys: Vec<f64> = d
    .entities()
    .filter(|e| matches!(e.specific, EntityType::LwPolyline(_)))
    .map(poly_area)
    .collect();
  assert_eq!(polys.len(), 2, "outer + hole must both be emitted");
  assert!(
    polys[0] < polys[1],
    "hole (area {:.0}) must be cut before the outer contour (area {:.0})",
    polys[0],
    polys[1]
  );

  // R3-2: an engraved assembly label "1" on the Engrave layer (ACI 5).
  let text = d
    .entities()
    .find_map(|e| match &e.specific {
      EntityType::Text(t) => Some((t.value.clone(), e.common.layer.clone(), e.common.color.index())),
      _ => None,
    })
    .expect("an engraved TEXT label");
  assert_eq!(text.0, "1", "label is the 1-based assembly number");
  assert_eq!(text.1, "Engrave", "engraving goes on the Engrave layer");
  assert_eq!(text.2, Some(5), "engrave colour is ACI 5");

  let engrave_layer = d.layers().find(|l| l.name == "Engrave").expect("Engrave layer registered");
  assert_eq!(engrave_layer.color.index(), Some(5));
}
