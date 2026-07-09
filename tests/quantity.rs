//! R3-1 (part quantity / copies) and R1-1a (single `Cut` layer + colour).
//!
//! A single square piece with `quantity = 3` must produce three nesting items,
//! three placements all resolving to the same source piece, and three emitted
//! outlines. Every emitted entity must land on the `Cut` layer with ACI 1 (red).

use std::path::PathBuf;

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use twister_splitter::emit::emit;
use twister_splitter::extract::{Piece, PieceKind, PieceSource};
use twister_splitter::geom::Bbox;
use twister_splitter::nest;

/// A 20×20 square, closed LwPolyline, as one loose piece with `quantity` copies.
fn square_piece(quantity: usize) -> Piece {
  let mut lw = LwPolyline::default();
  for &(x, y) in &[(0.0, 0.0), (20.0, 0.0), (20.0, 20.0), (0.0, 20.0)] {
    lw.vertices.push(LwPolylineVertex { x, y, ..Default::default() });
  }
  lw.set_is_closed(true);
  Piece {
    label: "sq".into(),
    kind: PieceKind::Loose(vec![Entity::new(EntityType::LwPolyline(lw))]),
    bbox: Bbox { min_x: 0.0, min_y: 0.0, max_x: 20.0, max_y: 20.0 },
    area: 400.0,
    source: PieceSource::Part,
    id: 1,
    quantity,
  }
}

#[test]
fn quantity_reserves_and_places_one_item_per_copy() {
  // LWPOLYLINE is an R2000+ entity; emit inherits the source version, so a bare
  // R12 `Drawing::new()` would drop every emitted polyline on save.
  let mut drawing = Drawing::new();
  drawing.header.version = dxf::enums::AcadVersion::R2013;
  let pieces = vec![square_piece(3)];

  // R3-1: build_items expands to one nesting item per copy, all piece_index 0.
  let items = nest::build_items(&drawing, &pieces);
  assert_eq!(items.len(), 3, "quantity 3 must yield 3 nesting items");
  assert!(items.iter().all(|it| it.piece_index == 0), "every copy points at the source piece");

  // Nest onto a generous sheet: all three copies fit on sheet 0, none oversized.
  let result = nest::nest(&items, 400.0, 400.0, 2.0, 0.0, None, |_, _| {}).expect("nest");
  assert!(result.oversized.is_empty(), "a 20mm square is not oversized");
  assert_eq!(result.placed.len(), 3, "three copies must be placed");
  assert!(result.placed.iter().all(|p| p.piece_index == 0), "all placements resolve to piece 0");

  // Emit and reload: three cut outlines, each on the `Cut` layer with ACI 1.
  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-quantity");
  let _ = std::fs::remove_dir_all(&out_dir);
  let report = emit(&drawing, &pieces, &result.placed, &out_dir, "sq", 0.0).expect("emit");
  assert_eq!(report.files.len(), 1, "all copies fit on one sheet");

  let d = Drawing::load_file(&report.files[0]).expect("reload");
  let cut: Vec<_> = d
    .entities()
    .filter(|e| matches!(e.specific, EntityType::LwPolyline(_)))
    .collect();
  assert_eq!(cut.len(), 3, "three square copies must be emitted");
  for e in &cut {
    assert_eq!(e.common.layer, "Cut", "emitted geometry must be on the Cut layer");
    assert_eq!(e.common.color.index(), Some(1), "cut colour must be ACI 1 (red)");
  }

  // R1-1a: the Cut layer exists in the layer table with the cut colour.
  let cut_layer = d.layers().find(|l| l.name == "Cut").expect("Cut layer registered");
  assert_eq!(cut_layer.color.index(), Some(1), "Cut layer colour must be ACI 1");
}
