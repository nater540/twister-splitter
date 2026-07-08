//! Write one output DXF per packed sheet, transforming each piece into place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use dxf::Drawing;
use dxf::entities::{Entity, EntityType};

use crate::extract::{Piece, PieceKind};
use crate::geom::Affine;

/// Apply a rotation+translation to an entity's geometry, in place.
fn transform_entity(entity: &mut Entity, xf: &Affine) {
  match &mut entity.specific {
    EntityType::Spline(s) => {
      for p in &mut s.control_points {
        let (x, y) = xf.apply(p.x, p.y);
        p.x = x;
        p.y = y;
      }
      for p in &mut s.fit_points {
        let (x, y) = xf.apply(p.x, p.y);
        p.x = x;
        p.y = y;
      }
      // Tangents are directions, not positions: rotate only.
      let (tx, ty) = xf.apply_vec(s.start_tangent.x, s.start_tangent.y);
      s.start_tangent.x = tx;
      s.start_tangent.y = ty;
      let (ex, ey) = xf.apply_vec(s.end_tangent.x, s.end_tangent.y);
      s.end_tangent.x = ex;
      s.end_tangent.y = ey;
    }
    EntityType::LwPolyline(p) => {
      for v in &mut p.vertices {
        let (x, y) = xf.apply(v.x, v.y);
        v.x = x;
        v.y = y;
      }
    }
    EntityType::Polyline(p) => {
      for v in p.vertices_mut() {
        let (x, y) = xf.apply(v.location.x, v.location.y);
        v.location.x = x;
        v.location.y = y;
      }
    }
    EntityType::Line(l) => {
      let (x1, y1) = xf.apply(l.p1.x, l.p1.y);
      l.p1.x = x1;
      l.p1.y = y1;
      let (x2, y2) = xf.apply(l.p2.x, l.p2.y);
      l.p2.x = x2;
      l.p2.y = y2;
    }
    EntityType::Circle(c) => {
      let (x, y) = xf.apply(c.center.x, c.center.y);
      c.center.x = x;
      c.center.y = y;
      // radius is rotation/translation-invariant.
    }
    EntityType::Arc(a) => {
      let (x, y) = xf.apply(a.center.x, a.center.y);
      a.center.x = x;
      a.center.y = y;
      // Sweep angles are measured CCW from +X, so they rotate with the arc.
      let deg = xf.rotation().to_degrees();
      a.start_angle += deg;
      a.end_angle += deg;
    }
    EntityType::Ellipse(e) => {
      let (cx, cy) = xf.apply(e.center.x, e.center.y);
      e.center.x = cx;
      e.center.y = cy;
      // major_axis is a direction relative to the centre: rotate only. The
      // ratio and start/end parameters are relative to it and stay put.
      let (mx, my) = xf.apply_vec(e.major_axis.x, e.major_axis.y);
      e.major_axis.x = mx;
      e.major_axis.y = my;
    }
    _ => {}
  }
}

/// A piece assigned to a sheet with the transform that maps its geometry from
/// its local frame onto that sheet. Both packers (rectangle and nesting) emit
/// this common form, so `emit` is agnostic to how placement was computed.
pub struct Placed {
  pub piece_index: usize,
  pub sheet: usize,
  pub transform: Affine,
  pub oversized: bool,
}

/// Result of writing all sheets.
pub struct EmitReport {
  pub files: Vec<PathBuf>,
}

/// Write every sheet described by `placed` into `out_dir`.
///
/// `source` is the original drawing, used to copy block definitions that
/// INSERT pieces depend on.
pub fn emit(
  source: &Drawing,
  pieces: &[Piece],
  placed: &[Placed],
  out_dir: &Path,
  file_stem: &str,
) -> std::io::Result<EmitReport> {
  std::fs::create_dir_all(out_dir)?;

  let sheet_count = placed.iter().map(|p| p.sheet).max().map_or(0, |m| m + 1);
  let mut files = Vec::new();

  for sheet in 0..sheet_count {
    let mut out = Drawing::new();
    // Inherit the source's ACAD version. `Drawing::new()` defaults to R12,
    // whose writer silently drops SPLINE (an R13+ entity) — so without this
    // every spline outline would vanish from the output.
    out.header.version = source.header.version;
    let mut added_blocks: HashSet<String> = HashSet::new();

    for placement in placed.iter().filter(|p| p.sheet == sheet) {
      let piece = &pieces[placement.piece_index];
      let xf = &placement.transform;

      match &piece.kind {
        PieceKind::Loose(entities) => {
          for entity in entities {
            let mut e = entity.clone();
            transform_entity(&mut e, xf);
            out.add_entity(e);
          }
        }
        PieceKind::Insert { insert, block_name } => {
          // Locate the referenced block once, then reuse it below.
          let block = source.blocks().find(|b| &b.name == block_name);
          // Copy the block definition into this sheet on first use.
          if added_blocks.insert(block_name.clone())
            && let Some(b) = block {
              out.add_block(b.clone());
            }
          let mut e = (**insert).clone();
          if let EntityType::Insert(ins) = &mut e.specific {
            // Position the block reference so a block vertex `v` renders at
            // `xf(v)`: with insert rotation R and location L, the reference maps
            // `v -> L + R*(v - base)`, so R = xf's rotation and L = xf(base).
            let base = block
              .map(|b| (b.base_point.x, b.base_point.y))
              .unwrap_or((0.0, 0.0));
            let (lx, ly) = xf.apply(base.0, base.1);
            ins.location.x = lx;
            ins.location.y = ly;
            ins.rotation = xf.rotation().to_degrees();
          }
          out.add_entity(e);
        }
      }
    }

    out.normalize();
    let path = out_dir.join(format!("{file_stem}_sheet_{sheet:02}.dxf"));
    out
      .save_file(&path)
      .map_err(|e| std::io::Error::other(e.to_string()))?;
    files.push(path);
  }

  Ok(EmitReport { files })
}
