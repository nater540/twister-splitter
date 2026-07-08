//! Regression test for non-spline cut geometry (LINE / CIRCLE / …): it must be
//! recognised as pieces, sized correctly, and repositioned in the output.

use dxf::entities::{Circle, Entity, EntityType, Line};
use dxf::{Drawing, Point};

use twister_splitter::emit::emit;
use twister_splitter::extract::{extract, Sources};
use twister_splitter::pack::{pack, PackConfig};

fn on_layer(specific: EntityType, layer: &str) -> Entity {
  let mut e = Entity::new(specific);
  e.common.layer = layer.to_string();
  e
}

#[test]
fn circle_and_line_geometry_survives_and_moves() {
  let mut drawing = Drawing::new();
  drawing.header.version = dxf::enums::AcadVersion::R2018;

  // A circle far from the origin, on its own layer.
  drawing.add_entity(on_layer(
    EntityType::Circle(Circle::new(Point::new(900.0, 900.0, 0.0), 20.0)),
    "hole",
  ));
  // A line on another layer.
  drawing.add_entity(on_layer(
    EntityType::Line(Line::new(
      Point::new(800.0, 800.0, 0.0),
      Point::new(850.0, 830.0, 0.0),
    )),
    "edge",
  ));

  let pieces = extract(&drawing, Sources::Both);
  assert_eq!(pieces.len(), 2, "circle and line should each form a piece");

  // The circle forms its own part with the full 40x40 bbox (r=20), not
  // empty/undersized. (Pieces are labelled `part:N` under per-part extraction.)
  let hole = pieces
    .iter()
    .find(|p| (p.bbox.width() - 40.0).abs() < 1e-6 && (p.bbox.height() - 40.0).abs() < 1e-6)
    .expect("circle part with 40x40 bbox");
  assert!((hole.bbox.width() - 40.0).abs() < 1e-9);
  assert!((hole.bbox.height() - 40.0).abs() < 1e-9);

  let cfg = PackConfig {
    sheet_w: 400.0,
    sheet_h: 400.0,
    kerf: 1.0,
    allow_rotation: true,
  };
  let bboxes: Vec<_> = pieces.iter().map(|p| p.bbox).collect();
  let placements = pack(&bboxes, &cfg);
  assert!(placements.iter().all(|p| !p.oversized));

  let out_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-geo");
  let _ = std::fs::remove_dir_all(&out_dir);
  let placed: Vec<_> = placements
    .iter()
    .map(|p| p.to_placed(&bboxes[p.piece_index]))
    .collect();
  let report = emit(&drawing, &pieces, &placed, &out_dir, "geo").expect("emit");

  // Reload and confirm the circle survives, keeps its radius, and was moved
  // onto the sheet (its original centre at 900,900 is off any 400mm sheet).
  let mut found_circle = false;
  for f in &report.files {
    let d = Drawing::load_file(f).unwrap();
    for e in d.entities() {
      if let EntityType::Circle(c) = &e.specific {
        found_circle = true;
        assert!((c.radius - 20.0).abs() < 1e-9, "radius must be preserved");
        assert!(
          c.center.x >= 0.0 && c.center.x <= 400.0 && c.center.y >= 0.0 && c.center.y <= 400.0,
          "circle centre must land on the sheet, got ({},{})",
          c.center.x,
          c.center.y
        );
      }
    }
  }
  assert!(found_circle, "circle must appear in the output");
}
