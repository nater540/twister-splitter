//! R3-3 (micro-tabs / holding bridges): a closed outline is emitted as open
//! polyline segments separated by uncut gaps, so the total cut length drops by
//! exactly `tab_count × tab_width` and the part stays attached to the sheet.

use std::path::PathBuf;

use dxf::entities::{Entity, EntityType, LwPolyline};
use dxf::{Drawing, LwPolylineVertex};

use twister_splitter::emit::{emit_opts, EmitOptions, Placed};
use twister_splitter::extract::{Piece, PieceKind, PieceSource};
use twister_splitter::geom::{Affine, Bbox};

fn square_piece(side: f64) -> Piece {
  let mut lw = LwPolyline::default();
  for &(x, y) in &[(0.0, 0.0), (side, 0.0), (side, side), (0.0, side)] {
    lw.vertices.push(LwPolylineVertex { x, y, ..Default::default() });
  }
  lw.set_is_closed(true);
  Piece {
    label: "sq".into(),
    kind: PieceKind::Loose(vec![Entity::new(EntityType::LwPolyline(lw))]),
    bbox: Bbox { min_x: 0.0, min_y: 0.0, max_x: side, max_y: side },
    area: side * side,
    source: PieceSource::Part,
    id: 1,
    quantity: 1,
  }
}

/// Open-path length of an LwPolyline (does not close the last→first edge).
fn open_len(e: &Entity) -> f64 {
  let EntityType::LwPolyline(p) = &e.specific else { return 0.0 };
  let v = &p.vertices;
  (1..v.len()).map(|i| (v[i].x - v[i - 1].x).hypot(v[i].y - v[i - 1].y)).sum()
}

#[test]
fn tabs_leave_uncut_gaps_summing_to_tab_count_times_width() {
  let mut drawing = Drawing::new();
  drawing.header.version = dxf::enums::AcadVersion::R2013;

  let side = 20.0;
  let piece = square_piece(side);
  let placed = Placed {
    piece_index: 0,
    sheet: 0,
    transform: Affine::place(&piece.bbox, 0.0, 0.0, 0.0),
    oversized: false,
    locked: false,
  };

  let (tab_width, tab_count) = (4.0, 4);
  let opts = EmitOptions { tab_width, tab_count, ..Default::default() };
  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tabs");
  let _ = std::fs::remove_dir_all(&out_dir);
  let report = emit_opts(&drawing, &[piece], &[placed], &out_dir, "sq", opts).expect("emit");

  let d = Drawing::load_file(&report.files[0]).expect("reload");
  let segs: Vec<&Entity> = d
    .entities()
    .filter(|e| matches!(e.specific, EntityType::LwPolyline(_)))
    .collect();

  // Four gaps → four open cut segments, none of them closed.
  assert_eq!(segs.len(), tab_count, "one open cut segment between each pair of tabs");
  for e in &segs {
    let EntityType::LwPolyline(p) = &e.specific else { unreachable!() };
    assert!(!p.is_closed(), "tabbed segments must be open polylines");
  }

  // Cut length = perimeter − tab_count·tab_width (the uncut bridges).
  let perimeter = 4.0 * side;
  let cut: f64 = segs.iter().map(|e| open_len(e)).sum();
  let expected = perimeter - tab_count as f64 * tab_width;
  assert!(
    (cut - expected).abs() < 1e-6,
    "cut length {cut:.3} should equal perimeter − tabs = {expected:.3}"
  );
}
