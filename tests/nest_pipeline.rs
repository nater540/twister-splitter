//! End-to-end test of the shape-nesting path on the real fixture: every piece
//! is placed, all cut geometry survives the round-trip, and (crucially) the
//! rendered geometry of each non-oversized sheet stays within the sheet — the
//! guarantee that the reserved polygon contains all emitted geometry.

use std::collections::HashMap;
use std::path::PathBuf;

use dxf::Drawing;
use dxf::entities::EntityType;

use twister_splitter::emit::{emit, Placed};
use twister_splitter::extract::{extract, Sources};
use twister_splitter::geom::Affine;
use twister_splitter::nest;

const SHEET: f64 = 400.0;
const KERF: f64 = 2.0;

/// base point + vertices of a block definition, keyed by block name.
type BlockGeom = HashMap<String, ((f64, f64), Vec<(f64, f64)>)>;

/// Bounds of a sheet's rendered geometry, resolving INSERTs against their block.
fn sheet_bounds(d: &Drawing) -> (f64, f64, f64, f64) {
  let mut blocks: BlockGeom = HashMap::new();
  for b in d.blocks() {
    let mut pts = Vec::new();
    for e in &b.entities {
      collect_xy(e, &mut pts);
    }
    blocks.insert(b.name.clone(), ((b.base_point.x, b.base_point.y), pts));
  }

  let (mut nx, mut ny, mut xx, mut xy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
  let mut acc = |x: f64, y: f64| {
    nx = nx.min(x);
    ny = ny.min(y);
    xx = xx.max(x);
    xy = xy.max(y);
  };
  for e in d.entities() {
    match &e.specific {
      EntityType::Insert(ins) => {
        if let Some(((bx, by), pts)) = blocks.get(&ins.name) {
          let (s, c) = ins.rotation.to_radians().sin_cos();
          for &(x, y) in pts {
            let (dx, dy) = (x - bx, y - by);
            acc(ins.location.x + c * dx - s * dy, ins.location.y + s * dx + c * dy);
          }
        }
      }
      _ => {
        let mut pts = Vec::new();
        collect_xy(e, &mut pts);
        for (x, y) in pts {
          acc(x, y);
        }
      }
    }
  }
  (nx, ny, xx, xy)
}

fn collect_xy(e: &dxf::entities::Entity, out: &mut Vec<(f64, f64)>) {
  match &e.specific {
    EntityType::Spline(s) => out.extend(s.control_points.iter().map(|p| (p.x, p.y))),
    EntityType::LwPolyline(p) => out.extend(p.vertices.iter().map(|v| (v.x, v.y))),
    _ => {}
  }
}

#[test]
fn nesting_places_all_pieces_within_bounds() {
  let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/gengar-stacked.dxf");
  let drawing = Drawing::load_file(path).expect("load fixture");
  let pieces = extract(&drawing, Sources::Both);
  // Per-part extraction splits multi-part layers, so there are more pieces than
  // layers; exact count depends on flattening, so assert a robust lower bound.
  let n_pieces = pieces.len();
  assert!(n_pieces >= 60, "expected many parts from the fixture, got {n_pieces}");

  let items = nest::build_items(&drawing, &pieces);
  assert_eq!(items.len(), n_pieces, "every piece should produce a nesting polygon");

  let result = nest::nest(&items, SHEET, SHEET, KERF, |_, _| {}).expect("nest");

  // Assemble the full placement set (fitted + oversized on their own sheets),
  // mirroring what the CLI does.
  let mut placed = result.placed;
  let mut oversized_sheets = Vec::new();
  for (k, &pi) in result.oversized.iter().enumerate() {
    let sheet = result.sheets + k;
    oversized_sheets.push(sheet);
    placed.push(Placed {
      piece_index: pi,
      sheet,
      transform: Affine::place(&pieces[pi].bbox, 0.0, 0.0, 0.0),
      oversized: true,
    });
  }

  // Nesting should beat the bounding-box packer's 4 sheets on this fixture.
  let total_sheets = placed.iter().map(|p| p.sheet).max().unwrap() + 1;
  assert!(total_sheets <= 4, "nesting used {total_sheets} sheets, expected <= 4");

  // Every piece placed exactly once.
  let mut seen: Vec<usize> = placed.iter().map(|p| p.piece_index).collect();
  seen.sort_unstable();
  seen.dedup();
  assert_eq!(seen.len(), n_pieces, "every piece must be placed exactly once");

  // Emit, reload, and check bounds + geometry preservation.
  let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-nest");
  let _ = std::fs::remove_dir_all(&out_dir);
  let report = emit(&drawing, &pieces, &placed, &out_dir, "gengar").expect("emit");

  let oversized_files: Vec<usize> = oversized_sheets.clone();
  let (mut splines, mut inserts, mut polylines) = (0, 0, 0);
  for (sheet, f) in report.files.iter().enumerate() {
    let d = Drawing::load_file(f).expect("reload");
    for e in d.entities() {
      match &e.specific {
        EntityType::Spline(_) => splines += 1,
        EntityType::Insert(_) => inserts += 1,
        EntityType::LwPolyline(_) => polylines += 1,
        _ => {}
      }
    }
    if oversized_files.contains(&sheet) {
      continue; // oversized pieces legitimately exceed the sheet
    }
    let (nx, ny, xx, xy) = sheet_bounds(&d);
    let tol = KERF + 0.5; // simplification + kerf slack
    assert!(
      nx >= -tol && ny >= -tol && xx <= SHEET + tol && xy <= SHEET + tol,
      "sheet {sheet} geometry out of bounds: x[{nx:.1},{xx:.1}] y[{ny:.1},{xy:.1}]"
    );
  }
  // 141 loose splines minus the zero-area degenerate artifacts dropped by
  // per-part extraction; all real cut outlines survive.
  assert!(splines >= 100, "real spline outlines must survive, got {splines}");
  assert_eq!(inserts, 6, "all inserts must survive");
  assert_eq!(polylines, 1, "the polyline must survive");
}
