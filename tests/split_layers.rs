//! R1-1b (outer-cut vs inner-cut layer split, loose parts only): a framed part
//! (outer contour + hole) must emit its outer contour on `Cut-Outer` and its
//! hole on `Cut-Inner`, each with its own colour, and register both layers.

use std::path::PathBuf;

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use twister_splitter::emit::{emit_opts, EmitOptions, Placed};
use twister_splitter::extract::{Piece, PieceKind, PieceSource};
use twister_splitter::geom::{Affine, Bbox};

fn square(ox: f64, oy: f64, s: f64) -> Entity {
  let mut lw = LwPolyline::default();
  for &(x, y) in &[(0.0, 0.0), (s, 0.0), (s, s), (0.0, s)] {
    lw.vertices.push(LwPolylineVertex { x: ox + x, y: oy + y, ..Default::default() });
  }
  lw.set_is_closed(true);
  Entity::new(EntityType::LwPolyline(lw))
}

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
fn outer_and_inner_contours_go_to_separate_layers() {
  let mut drawing = Drawing::new();
  drawing.header.version = dxf::enums::AcadVersion::R2013;

  // 20×20 outer square with a 6×6 hole — one loose part.
  let piece = Piece {
    label: "framed".into(),
    kind: PieceKind::Loose(vec![square(0.0, 0.0, 20.0), square(7.0, 7.0, 6.0)]),
    bbox: Bbox { min_x: 0.0, min_y: 0.0, max_x: 20.0, max_y: 20.0 },
    area: 400.0 - 36.0,
    source: PieceSource::Part,
    id: 1,
    quantity: 1,
  };
  let placed = Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine::place(&piece.bbox, 0.0, 0.0, 0.0),
    oversized: false,
    locked: false,
  };

  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-split");
  let _ = std::fs::remove_dir_all(&out_dir);
  let opts = EmitOptions { split_cut_layers: true, ..Default::default() };
  emit_opts(&drawing, &[piece], &[placed], &out_dir, "framed", opts).expect("emit");

  let d = Drawing::load_file(out_dir.join("framed_sheet_00.dxf")).expect("reload");
  let polys: Vec<&Entity> = d
    .entities()
    .filter(|e| matches!(e.specific, EntityType::LwPolyline(_)))
    .collect();
  assert_eq!(polys.len(), 2, "outer + hole");

  // The larger polyline is the outer contour, the smaller is the hole.
  let outer = polys.iter().max_by(|a, b| poly_area(a).total_cmp(&poly_area(b))).unwrap();
  let inner = polys.iter().min_by(|a, b| poly_area(a).total_cmp(&poly_area(b))).unwrap();

  assert_eq!(outer.common.layer, "Cut-Outer", "outer contour on Cut-Outer");
  assert_eq!(outer.common.color.index(), Some(1), "outer colour ACI 1");
  assert_eq!(inner.common.layer, "Cut-Inner", "hole on Cut-Inner");
  assert_eq!(inner.common.color.index(), Some(3), "inner colour ACI 3");

  // Both split layers registered in the table with their colours.
  let lo = d.layers().find(|l| l.name == "Cut-Outer").expect("Cut-Outer registered");
  let li = d.layers().find(|l| l.name == "Cut-Inner").expect("Cut-Inner registered");
  assert_eq!(lo.color.index(), Some(1));
  assert_eq!(li.color.index(), Some(3));
}
